import AppKit
@testable import AppsMarketCore
import AppsMarketCrypto
import AppsMarketTestSupport
import Foundation
import Testing

@MainActor
final class TestPresenter: UpdatePresenting {
    private(set) var presentations = 0

    func presentUpdates(for updater: Updater) {
        presentations += 1
    }
}

@MainActor
final class TestDelegate: UpdaterDelegate {
    var answer = true
    private(set) var questions = 0

    func updaterShouldRelaunch(_ updater: Updater) async -> Bool {
        questions += 1
        return answer
    }
}

@MainActor
final class ObservedProgress {
    var values: [Double] = []
}

@Suite("Updater")
@MainActor
struct UpdaterTests {
    let world: UpdateWorld

    init() throws {
        world = try UpdateWorld()
    }

    /// Updater sans vérification automatique : chaque test décide quand vérifier.
    func manualUpdater(installer: (any UpdateInstaller)? = nil, allowsAnonymousStatistics: Bool = true) throws -> Updater {
        world.defaults.set(false, forKey: Updater.Keys.automaticChecks)
        return try world.makeUpdater(installer: installer, allowsAnonymousStatistics: allowsAnonymousStatistics)
    }

    func exists(_ url: URL) -> Bool {
        FileManager.default.fileExists(atPath: url.path(percentEncoded: false))
    }

    // MARK: - Réglages

    @Test("Réglages par défaut, puis persistés dans les préférences de l'app")
    func settings() throws {
        defer { world.cleanUp() }
        let updater = try world.makeUpdater()
        #expect(updater.automaticChecks)
        #expect(updater.checkInterval == 86_400)
        #expect(!updater.installsOnQuit)
        #expect(updater.channel == .stable)
        #expect(updater.sharesAnonymousStatistics)
        #expect(updater.phase == .idle)
        #expect(updater.currentVersion?.description == "1.0.0")
        #expect(updater.lastCheckDate == nil)
        #expect(updater.canCheckForUpdates)

        updater.automaticChecks = false
        updater.checkInterval = 7 * 86_400
        updater.installsOnQuit = true
        updater.channel = .beta
        updater.sharesAnonymousStatistics = false

        let reopened = try world.makeUpdater()
        #expect(!reopened.automaticChecks)
        #expect(reopened.checkInterval == 7 * 86_400)
        #expect(reopened.installsOnQuit)
        #expect(reopened.channel == .beta)
        #expect(!reopened.sharesAnonymousStatistics)
        #expect(world.defaults.string(forKey: "AppsMarketKit.channel") == "beta")
    }

    @Test("Identifiant d'installation : UUID v4 tiré au lancement, envoyé seulement avec les statistiques")
    func installID() async throws {
        defer { world.cleanUp() }
        world.withdrawAll()
        let updater = try manualUpdater()
        updater.start()
        let stored = try #require(world.defaults.string(forKey: Updater.Keys.installID))
        let id = try #require(UUID(uuidString: stored))
        #expect(stored == stored.lowercased())
        // Version 4 (aléatoire) et variante RFC 9562.
        #expect(Array(stored)[14] == "4")
        #expect("89ab".contains(Array(stored)[19]))

        await updater.checkNow().value
        #expect(world.server.requests.last?.value(forHTTPHeaderField: "X-AppsMarket-Install-Id") == id.uuidString.lowercased())
        #expect(try world.makeUpdater().sharesAnonymousStatistics)
        #expect(world.defaults.string(forKey: Updater.Keys.installID) == stored)

        // Coupées : l'identifiant disparaît et ne part plus. Rouvertes : un nouveau est tiré.
        updater.sharesAnonymousStatistics = false
        #expect(world.defaults.string(forKey: Updater.Keys.installID) == nil)
        await updater.checkNow().value
        #expect(world.server.requests.last?.value(forHTTPHeaderField: "X-AppsMarket-Install-Id") == nil)
        updater.sharesAnonymousStatistics = true
        await updater.checkNow().value
        let renewed = try #require(world.server.requests.last?.value(forHTTPHeaderField: "X-AppsMarket-Install-Id"))
        #expect(renewed != stored)
    }

    @Test("Une app sans statistiques ne crée ni n'envoie jamais d'identifiant")
    func noStatistics() async throws {
        defer { world.cleanUp() }
        world.withdrawAll()
        let updater = try manualUpdater(allowsAnonymousStatistics: false)
        #expect(!updater.sharesAnonymousStatistics)
        updater.sharesAnonymousStatistics = true
        updater.start()
        await updater.checkNow().value
        #expect(world.defaults.string(forKey: Updater.Keys.installID) == nil)
        #expect(world.server.requests.last?.value(forHTTPHeaderField: "X-AppsMarket-Install-Id") == nil)
    }

    // MARK: - Vérification

    @Test("Vérification au lancement, puis à intervalle ; arrêtée avec la vérification automatique")
    func scheduling() async throws {
        defer { world.cleanUp() }
        world.withdrawAll()
        world.clock.allow(2)
        let updater = try world.makeUpdater()
        updater.start()
        updater.start()
        try await world.wait { world.server.requests(to: world.checkPath).count == 3 && updater.phase == .upToDate }
        #expect(world.clock.sleeps == [.seconds(86_400), .seconds(86_400)])
        #expect(updater.lastCheckDate == world.clock.now)
        #expect(world.defaults.object(forKey: Updater.Keys.lastCheckDate) as? Date == world.clock.now)

        updater.automaticChecks = false
        try await Task.sleep(for: .milliseconds(50))
        #expect(world.server.requests(to: world.checkPath).count == 3)
    }

    @Test("Au lancement suivant, la vérification a lieu ; ensuite, l'intervalle court depuis la dernière")
    func intervalSinceLastCheck() async throws {
        defer { world.cleanUp() }
        world.withdrawAll()
        // Dernière vérification il y a une heure, intervalle d'un jour.
        world.defaults.set(world.clock.now.addingTimeInterval(-3_600), forKey: Updater.Keys.lastCheckDate)
        world.defaults.set(false, forKey: Updater.Keys.automaticChecks)
        world.clock.allow(1)
        let updater = try world.makeUpdater()
        updater.start()
        #expect(world.server.requests.isEmpty)
        // Rallumée, la vérification automatique attend le reste de l'intervalle.
        updater.automaticChecks = true
        try await world.wait { world.server.requests(to: world.checkPath).count == 1 }
        #expect(world.clock.sleeps == [.seconds(86_400 - 3_600)])
    }

    @Test("checkNow présente la fenêtre aussitôt, puis la version proposée")
    func checkNow() async throws {
        defer { world.cleanUp() }
        let publication = try world.publish(version: "1.1.0", notes: "## Nouveautés\n- Plus rapide")
        let presenter = TestPresenter()
        let updater = try manualUpdater()
        updater.presenter = presenter
        updater.start()
        let task = updater.checkNow()
        #expect(presenter.presentations == 1)
        await task.value
        #expect(updater.phase == .available)
        #expect(updater.availableUpdate == publication.response)
        #expect(updater.availableUpdate?.notes == "## Nouveautés\n- Plus rapide")
        #expect(presenter.presentations == 1)
        #expect(updater.lastCheckDate != nil)

        world.withdrawAll()
        await updater.checkNow().value
        #expect(updater.phase == .upToDate)
        #expect(updater.availableUpdate == nil)
    }

    @Test("Une vérification automatique présente une version trouvée, sauf une version ignorée")
    func skippedVersion() async throws {
        defer { world.cleanUp() }
        let publication = try world.publish(version: "1.1.0")
        let presenter = TestPresenter()
        world.clock.allow(0)
        let updater = try world.makeUpdater()
        updater.presenter = presenter
        updater.start()
        try await world.wait { updater.phase == .available }
        #expect(presenter.presentations == 1)

        updater.skip(publication.response.version)
        #expect(updater.phase == .idle)
        #expect(updater.availableUpdate == nil)
        #expect(updater.skippedVersion == publication.response.version)

        // Lancement suivant : la version ignorée n'est plus présentée…
        let reopened = try world.makeUpdater()
        let silent = TestPresenter()
        reopened.presenter = silent
        reopened.start()
        try await world.wait { world.server.requests(to: world.checkPath).count == 2 && reopened.phase == .available }
        #expect(silent.presentations == 0)
        // … mais une vérification demandée la propose.
        await reopened.checkNow().value
        #expect(silent.presentations == 1)
        #expect(reopened.availableUpdate == publication.response)

        // Une version plus récente que celle ignorée est de nouveau présentée.
        try world.publish(version: "1.2.0")
        let later = try world.makeUpdater()
        let presented = TestPresenter()
        later.presenter = presented
        later.start()
        try await world.wait { later.availableUpdate?.version.description == "1.2.0" }
        #expect(presented.presentations == 1)
    }

    @Test("Échec d'une vérification demandée : l'erreur ; d'une vérification automatique : la version connue reste")
    func checkFailures() async throws {
        defer { world.cleanUp() }
        try world.publish(version: "1.1.0")
        let updater = try manualUpdater()
        updater.start()
        await updater.checkNow().value
        #expect(updater.phase == .available)

        world.server.route(world.checkPath, response: FakeServer.Response(status: 503))
        await updater.checkNow().value
        #expect(updater.phase == .failed(.server(status: 503, message: nil)))
        #expect(updater.availableUpdate != nil)

        // Un jour plus tard, la vérification automatique échoue à son tour, sans effacer la version.
        world.clock.allow(0)
        world.clock.advance(by: 86_400)
        updater.automaticChecks = true
        try await world.wait { world.server.requests(to: world.checkPath).count == 3 && !updater.isBusy }
        #expect(updater.phase == .available)
    }

    @Test("Réessayer refait ce qui a échoué : la vérification, sans installer la version déjà connue ; puis l'installation")
    func retry() async throws {
        defer { world.cleanUp() }
        let publication = try world.publish(version: "1.1.0")
        let offer = FakeServer.Response(
            status: 200, headers: ["Content-Type": "application/json"],
            body: try JSONSerialization.data(withJSONObject: publication.json)
        )
        let updater = try manualUpdater()
        updater.start()

        // Sans échec en cours, réessayer ne fait rien.
        await updater.retry().value
        #expect(world.server.requests.isEmpty)

        await updater.checkNow().value
        #expect(updater.phase == .available)

        // La vérification demandée échoue alors qu'une version est connue : réessayer vérifie de
        // nouveau, sans rien télécharger ni installer.
        world.server.route(world.checkPath, response: FakeServer.Response(status: 503))
        await updater.checkNow().value
        #expect(updater.phase == .failed(.server(status: 503, message: nil)))
        #expect(updater.availableUpdate != nil)
        await updater.retry().value
        #expect(updater.phase == .failed(.server(status: 503, message: nil)))
        #expect(world.server.requests(to: world.checkPath).count == 3)
        #expect(world.server.requests(to: publication.downloadPath).isEmpty)
        #expect(world.relauncher.relaunched.isEmpty)
        #expect(try FakeApps.version(of: world.installedApp) == "1.0.0")

        world.server.route(world.checkPath, response: offer)
        await updater.retry().value
        #expect(updater.phase == .available)

        // Le téléchargement échoue : réessayer reprend l'installation, jusqu'à la relance.
        world.server.route(publication.downloadPath, response: FakeServer.Response(status: 500))
        await updater.installAndRelaunch().value
        #expect(updater.phase == .failed(.server(status: 500, message: nil)))
        world.server.route(publication.downloadPath, response: FakeServer.Response(body: try Data(contentsOf: publication.archive)))
        await updater.retry().value
        #expect(world.server.requests(to: world.checkPath).count == 4)
        #expect(try FakeApps.version(of: world.installedApp) == "1.1.0")
        #expect(world.relauncher.relaunched == [world.installedApp])
    }

    // MARK: - Bout en bout

    @Test("Bout en bout : vérification, téléchargement, contrôles, échange et relance, par le vrai code d'installation")
    func endToEnd() async throws {
        defer { world.cleanUp() }
        let publication = try world.publish(version: "1.1.0")
        try FakeApps.quarantine(publication.archive)
        let updater = try manualUpdater()
        let delegate = TestDelegate()
        updater.delegate = delegate
        updater.start()

        await updater.checkNow().value
        #expect(updater.phase == .available)
        await updater.installAndRelaunch().value

        // Téléchargée dans Caches, préparée, échangée.
        #expect(world.server.requests(to: publication.downloadPath).count == 1)
        #expect(exists(world.caches.appending(path: "1.1.0.app.tar.gz")))
        #expect(delegate.questions == 1)
        #expect(try FakeApps.version(of: world.installedApp) == "1.1.0")
        #expect(!FakeApps.hasQuarantine(world.installedApp.appending(path: "Contents/Info.plist")))
        #expect(updater.phase == .installing)
        #expect(world.relauncher.relaunched == [world.installedApp])

        // La marque de relance précède la nouvelle instance, et l'ancien bundle attend le gate.
        let marker = try #require(world.defaults.dictionary(forKey: RelaunchMarker.key))
        #expect((marker["pid"] as? NSNumber)?.int32Value == ProcessInfo.processInfo.processIdentifier)
        let deadline = try #require(marker["deadline"] as? Date)
        #expect(deadline == world.clock.now.addingTimeInterval(20))
        let previousPath = try #require(world.defaults.string(forKey: RelaunchMarker.previousBundleKey))
        let previous = URL(filePath: previousPath, directoryHint: .isDirectory)
        #expect(try FakeApps.version(of: previous) == "1.0.0")
        #expect(world.defaults.object(forKey: Updater.Keys.preparedUpdate) == nil)

        // La nouvelle instance passe le gate, qui supprime l'ancien bundle. Ici, la marque est celle
        // de ce processus : il n'y a rien à attendre.
        _ = RelaunchGate.run(
            defaults: world.defaults, bundle: world.installedApp, bundleIdentifier: UpdateWorld.bundleIdentifier,
            timeout: .seconds(1), isRunning: RelaunchGate.isRunning
        )
        #expect(!exists(previous))
        #expect(exists(world.installedApp))
    }

    @Test("Installation à la fermeture : préparée en silence, échangée quand l'app se termine, sans relance")
    func installOnQuit() async throws {
        defer { world.cleanUp() }
        try world.publish(version: "1.1.0")
        let presenter = TestPresenter()
        world.defaults.set(true, forKey: Updater.Keys.installsOnQuit)
        world.clock.allow(0)
        let updater = try world.makeUpdater()
        updater.presenter = presenter
        updater.start()
        try await world.wait { updater.phase == .ready }
        #expect(presenter.presentations == 0)
        #expect(try FakeApps.version(of: world.installedApp) == "1.0.0")

        world.notifications.post(name: NSApplication.willTerminateNotification, object: nil)
        #expect(try FakeApps.version(of: world.installedApp) == "1.1.0")
        #expect(world.relauncher.relaunched.isEmpty)
        #expect(world.defaults.object(forKey: RelaunchMarker.key) == nil)
        let previous = try #require(world.defaults.string(forKey: RelaunchMarker.previousBundleKey))

        // Au lancement suivant, sans marque, le gate supprime l'ancien bundle aussitôt.
        #expect(RelaunchGate.run(
            defaults: world.defaults, bundle: world.installedApp, bundleIdentifier: UpdateWorld.bundleIdentifier,
            timeout: .seconds(1), isRunning: RelaunchGate.isRunning
        ) == .noMarker)
        #expect(!exists(URL(filePath: previous)))
    }

    @Test("Installation à la fermeture, même juste après l'échec d'une vérification")
    func installOnQuitAfterFailedCheck() async throws {
        defer { world.cleanUp() }
        try world.publish(version: "1.1.0")
        world.defaults.set(true, forKey: Updater.Keys.installsOnQuit)
        let updater = try manualUpdater()
        updater.start()
        await updater.checkNow().value
        try await world.wait { updater.phase == .ready }

        world.server.route(world.checkPath, response: FakeServer.Response(status: 500))
        await updater.checkNow().value
        #expect(updater.phase == .failed(.server(status: 500, message: nil)))
        world.notifications.post(name: NSApplication.willTerminateNotification, object: nil)
        #expect(try FakeApps.version(of: world.installedApp) == "1.1.0")
    }

    @Test("Une préparation déjà installée, retrouvée au lancement, est nettoyée")
    func stalePreparation() async throws {
        defer { world.cleanUp() }
        try world.publish(version: "1.1.0")
        let first = try manualUpdater()
        first.start()
        await first.checkNow().value
        await first.download().value
        let data = try #require(world.defaults.data(forKey: Updater.Keys.preparedUpdate))
        let json = try #require(try JSONSerialization.jsonObject(with: data) as? [String: Any])
        let prepared = try #require(json["prepared"] as? [String: Any])
        let path = try #require(prepared["workDirectory"] as? String)
        let workDirectory = try #require(URL(string: path))
        #expect(exists(workDirectory))

        // L'app tourne désormais en 1.1.0 : la préparation de la 1.1.0 ne sert plus.
        let app = InstalledApp(url: world.installedApp, bundleIdentifier: UpdateWorld.bundleIdentifier, version: try SemanticVersion.parseRelease("1.1.0"))
        let installer = LocalInstaller(app: app, publicKeys: try world.keyring, codeSignature: SimulatedCodeSignature(), systemVersion: ProcessInfo.processInfo.operatingSystemVersion)
        let setup = Updater.Setup(configuration: try UpdaterConfiguration(feed: world.feed, publicKeys: world.keyring), app: app, installer: installer)
        let environment = Updater.Environment(
            defaults: world.defaults, session: world.server.session(), cachesDirectory: world.caches,
            notificationCenter: world.notifications, relauncher: world.relauncher
        )
        let updated = Updater(setup: .success(setup), allowsAnonymousStatistics: true, environment: environment)
        updated.start()
        #expect(updated.phase == .idle)
        #expect(world.defaults.object(forKey: Updater.Keys.preparedUpdate) == nil)
        #expect(!exists(workDirectory))
        #expect((try? FileManager.default.contentsOfDirectory(atPath: world.caches.path(percentEncoded: false)))?.isEmpty ?? true)
    }

    @Test("Sans installation à la fermeture, quitter ne change rien")
    func quitWithoutInstallOnQuit() async throws {
        defer { world.cleanUp() }
        try world.publish(version: "1.1.0")
        let updater = try manualUpdater()
        updater.start()
        await updater.checkNow().value
        await updater.download().value
        #expect(updater.phase == .ready)
        world.notifications.post(name: NSApplication.willTerminateNotification, object: nil)
        #expect(try FakeApps.version(of: world.installedApp) == "1.0.0")
    }

    @Test("Le délégué peut refuser la relance : la mise à jour reste prête")
    func delegateRefuses() async throws {
        defer { world.cleanUp() }
        try world.publish(version: "1.1.0")
        let updater = try manualUpdater()
        let delegate = TestDelegate()
        delegate.answer = false
        updater.delegate = delegate
        updater.start()
        await updater.checkNow().value
        await updater.installAndRelaunch().value
        #expect(delegate.questions == 1)
        #expect(updater.phase == .ready)
        #expect(try FakeApps.version(of: world.installedApp) == "1.0.0")
        #expect(world.relauncher.relaunched.isEmpty)

        delegate.answer = true
        await updater.installAndRelaunch().value
        #expect(try FakeApps.version(of: world.installedApp) == "1.1.0")
        #expect(world.relauncher.relaunched.count == 1)
    }

    @Test("Une préparation survit à une relance de l'app ; sans son bundle, l'archive de Caches resert")
    func preparationSurvivesRestart() async throws {
        defer { world.cleanUp() }
        let publication = try world.publish(version: "1.1.0")
        let first = try manualUpdater()
        first.start()
        await first.checkNow().value
        await first.download().value
        #expect(first.phase == .ready)
        let requests = world.server.requests.count

        let second = try manualUpdater()
        second.start()
        #expect(second.phase == .ready)
        #expect(second.availableUpdate == publication.response)
        #expect(world.server.requests.count == requests)
        await second.installAndRelaunch().value
        #expect(try FakeApps.version(of: world.installedApp) == "1.1.0")
        #expect(world.relauncher.relaunched.count == 1)
    }

    @Test("Préparation perdue (dossier temporaire vidé) : elle est refaite depuis Caches, sans nouveau téléchargement")
    func lostPreparation() async throws {
        defer { world.cleanUp() }
        let publication = try world.publish(version: "1.1.0")
        let first = try manualUpdater()
        first.start()
        await first.checkNow().value
        await first.download().value
        let data = try #require(world.defaults.data(forKey: Updater.Keys.preparedUpdate))
        let prepared = try #require(String(data: data, encoding: .utf8))
        #expect(prepared.contains("\"version\":\"1.1.0\""))
        let workDirectory = try #require(try JSONSerialization.jsonObject(with: data) as? [String: Any])
        let preparedJSON = try #require(workDirectory["prepared"] as? [String: Any])
        let directory = try #require(preparedJSON["workDirectory"] as? String)
        try FileManager.default.removeItem(at: try #require(URL(string: directory)))

        let second = try manualUpdater()
        second.start()
        #expect(second.phase == .idle)
        await second.checkNow().value
        await second.download().value
        #expect(second.phase == .ready)
        #expect(world.server.requests(to: publication.downloadPath).count == 1)
    }

    @Test("Une version retirée entre-temps n'est pas installée")
    func withdrawnRelease() async throws {
        defer { world.cleanUp() }
        try world.publish(version: "1.1.0")
        let updater = try manualUpdater()
        updater.start()
        await updater.checkNow().value
        await updater.download().value
        #expect(updater.phase == .ready)

        world.withdrawAll()
        await updater.checkNow().value
        #expect(updater.phase == .upToDate)
        #expect(world.defaults.object(forKey: Updater.Keys.preparedUpdate) == nil)
        await updater.installAndRelaunch().value
        #expect(try FakeApps.version(of: world.installedApp) == "1.0.0")
        #expect(world.relauncher.relaunched.isEmpty)
    }

    @Test("Version retirée pendant le téléchargement (410) : oubliée avec son archive, la suite est une nouvelle vérification")
    func withdrawnDuringDownload() async throws {
        defer { world.cleanUp() }
        let publication = try world.publish(version: "1.1.0")
        let updater = try manualUpdater()
        updater.start()
        await updater.checkNow().value
        #expect(updater.phase == .available)

        // L'éditeur retire la version entre la vérification et le téléchargement.
        world.server.route(publication.downloadPath, response: FakeServer.Response(status: 410))
        await updater.installAndRelaunch().value
        #expect(updater.phase == .failed(.server(status: 410, message: nil)))
        #expect(updater.availableUpdate == nil)
        #expect((try? FileManager.default.contentsOfDirectory(atPath: world.caches.path(percentEncoded: false)))?.isEmpty ?? true)
        #expect(world.server.requests(to: publication.downloadPath).count == 1)

        // Plus rien à installer : réessayer l'installation ne retélécharge pas la même adresse.
        await updater.installAndRelaunch().value
        #expect(world.server.requests(to: publication.downloadPath).count == 1)
        #expect(updater.canCheckForUpdates)

        // « Réessayer », dans la fenêtre, relance donc une vérification : le serveur ne propose plus rien.
        world.withdrawAll()
        await updater.checkNow().value
        #expect(updater.phase == .upToDate)
        #expect(try FakeApps.version(of: world.installedApp) == "1.0.0")
        #expect(world.relauncher.relaunched.isEmpty)
    }

    @Test("Une version plus récente remplace celle qui était prête")
    func newerReplacesReady() async throws {
        defer { world.cleanUp() }
        try world.publish(version: "1.1.0")
        let updater = try manualUpdater()
        updater.start()
        await updater.checkNow().value
        await updater.download().value
        try world.publish(version: "1.2.0")
        await updater.checkNow().value
        #expect(updater.phase == .available)
        await updater.installAndRelaunch().value
        #expect(try FakeApps.version(of: world.installedApp) == "1.2.0")
    }

    // MARK: - Refus et échecs

    @Test("Archive signée par une autre clé : échec, et l'archive ne reste pas dans Caches")
    func foreignSignature() async throws {
        defer { world.cleanUp() }
        let stranger = TestSigner()
        try world.publish(version: "1.1.0", signer: stranger)
        let updater = try manualUpdater()
        updater.start()
        await updater.checkNow().value
        await updater.installAndRelaunch().value
        let expected = try world.signer.publicKey.keyID
        let found = try stranger.publicKey.keyID
        #expect(updater.phase == .failed(.signature(.keyIDMismatch(expected: expected, found: found))))
        #expect((try? FileManager.default.contentsOfDirectory(atPath: world.caches.path(percentEncoded: false)))?.isEmpty ?? true)
        #expect(try FakeApps.version(of: world.installedApp) == "1.0.0")
        #expect(world.relauncher.relaunched.isEmpty)
    }

    @Test("Bundle refusé par les contrôles : échec d'installation, rien d'échangé")
    func refusedBundle() async throws {
        defer { world.cleanUp() }
        try world.publish(version: "1.1.0", certificate: "Quelqu'un d'autre")
        let updater = try manualUpdater()
        updater.start()
        await updater.checkNow().value
        await updater.installAndRelaunch().value
        #expect(updater.phase == .failed(.installation(.codeSignature("certificat « Quelqu'un d'autre » refusé"))))
        #expect(try FakeApps.version(of: world.installedApp) == "1.0.0")
    }

    @Test("App signée ad hoc : la version est trouvée, mais indisponible")
    func unavailableInstallation() async throws {
        let adHoc = try UpdateWorld(certificate: nil)
        defer { adHoc.cleanUp() }
        try adHoc.publish(version: "1.1.0")
        adHoc.defaults.set(false, forKey: Updater.Keys.automaticChecks)
        let updater = try adHoc.makeUpdater()
        updater.start()
        await updater.checkNow().value
        #expect(updater.phase == .unavailable(.adHocSignature))
        #expect(updater.availableUpdate?.version.description == "1.1.0")
        await updater.installAndRelaunch().value
        #expect(updater.phase == .unavailable(.adHocSignature))
        #expect(adHoc.server.requests.count == 1)
    }

    @Test("Configuration absente : indisponible, sans aucune requête")
    func misconfigured() async throws {
        defer { world.cleanUp() }
        let environment = Updater.Environment(
            defaults: world.defaults, session: world.server.session(), cachesDirectory: world.caches,
            notificationCenter: world.notifications, relauncher: world.relauncher
        )
        let updater = Updater(setup: .failure(.missingPublicKeys), allowsAnonymousStatistics: true, environment: environment)
        #expect(updater.phase == .unavailable(.misconfigured("DrakBasePublicKeys absent de l'Info.plist")))
        #expect(!updater.isConfigured)
        #expect(!updater.canCheckForUpdates)
        updater.start()
        await updater.checkNow().value
        #expect(world.server.requests.isEmpty)
        #expect(UpdateUnavailability.misconfigured("DrakBasePublicKeys absent de l'Info.plist").message
            == "Les mises à jour ne sont pas configurées dans cette app : DrakBasePublicKeys absent de l'Info.plist.")
    }

    @Test("Avec DrakBaseInstallerService, l'installation attend le service XPC (C2)")
    func installerService() throws {
        defer { world.cleanUp() }
        let configuration = try UpdaterConfiguration(feed: world.feed, publicKeys: world.keyring, installerService: "fr.example.essai.Installer")
        let installer = Updater.defaultInstaller(app: world.app, configuration: configuration)
        #expect(installer.unavailability() == .installerServiceUnsupported("fr.example.essai.Installer"))
        let local = Updater.defaultInstaller(app: world.app, configuration: try UpdaterConfiguration(feed: world.feed, publicKeys: world.keyring))
        #expect(local is LocalInstaller)
    }

    @Test("Relance impossible : l'échec est montré, puis seule la relance est retentée, sans rien réinstaller")
    func relaunchFailure() async throws {
        defer { world.cleanUp() }
        let publication = try world.publish(version: "1.1.0")
        world.relauncher.failure = .relaunch("la nouvelle instance n'est pas apparue")
        let updater = try manualUpdater()
        updater.start()
        await updater.checkNow().value
        await updater.installAndRelaunch().value
        #expect(updater.phase == .failed(.relaunch("la nouvelle instance n'est pas apparue")))
        #expect(try FakeApps.version(of: world.installedApp) == "1.1.0")
        // La marque reste et s'éteint seule : une instance en retard attendrait encore celle-ci.
        #expect(world.defaults.object(forKey: RelaunchMarker.key) != nil)

        // L'app tourne encore en 1.0.0 sur une 1.1.0 installée : plus de vérification ni d'installation.
        #expect(!updater.canCheckForUpdates)
        let requests = world.server.requests.count
        await updater.checkNow().value
        #expect(world.server.requests.count == requests)
        world.relauncher.failure = nil
        await updater.installAndRelaunch().value
        #expect(world.relauncher.relaunched == [world.installedApp, world.installedApp])
        #expect(world.server.requests(to: publication.downloadPath).count == 1)
        #expect(updater.phase == .installing)
    }

    @Test("Relance impossible, vérification automatique active : elle s'arrête, au lieu de tourner sans pause")
    func schedulerStopsAfterRelaunchFailure() async throws {
        defer { world.cleanUp() }
        try world.publish(version: "1.1.0")
        world.relauncher.failure = .relaunch("la nouvelle instance n'est pas apparue")
        // Aucune attente n'aboutit : après la vérification du lancement, le planificateur dort.
        world.clock.allow(0)
        let updater = try world.makeUpdater()
        updater.start()
        try await world.wait { updater.phase == .available }
        await updater.installAndRelaunch().value
        #expect(updater.phase == .failed(.relaunch("la nouvelle instance n'est pas apparue")))
        #expect(try FakeApps.version(of: world.installedApp) == "1.1.0")
        #expect(updater.automaticChecks)

        // L'app reste ouverte sur l'ancienne version, bien au-delà de l'intervalle. Un réglage changé
        // relance le planificateur : chaque vérification serait écartée aussitôt, sans nouvelle
        // tentative notée, donc sans attente avant la suivante.
        world.clock.advance(by: 2 * 86_400)
        let reads = world.clock.reads
        let requests = world.server.requests.count
        updater.checkInterval = 3_600
        try await Task.sleep(for: .milliseconds(200))
        #expect(world.clock.reads - reads < 10)
        #expect(world.server.requests.count == requests)
        #expect(world.clock.sleeps.isEmpty)
    }

    @Test("Installation à la fermeture qui échoue : pas de nouvel essai en silence, l'échec est montré au lancement suivant")
    func failedInstallOnQuit() async throws {
        defer { world.cleanUp() }
        let publication = try world.publish(version: "1.1.0")
        world.defaults.set(true, forKey: Updater.Keys.installsOnQuit)
        world.clock.allow(0)
        let updater = try world.makeUpdater()
        updater.start()
        try await world.wait { updater.phase == .ready }

        let folder = world.applications.path(percentEncoded: false)
        try FileManager.default.setAttributes([.posixPermissions: 0o555], ofItemAtPath: folder)
        world.notifications.post(name: NSApplication.willTerminateNotification, object: nil)
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: folder)
        #expect(try FakeApps.version(of: world.installedApp) == "1.0.0")
        #expect(world.defaults.object(forKey: Updater.Keys.preparedUpdate) == nil)
        let failure = try #require(world.defaults.dictionary(forKey: Updater.Keys.quitInstallFailure))
        let message = try #require(failure["message"] as? String)
        #expect(message.hasPrefix("Le dossier de l'app"))

        // Lancement suivant : la version est présentée avec l'échec, sans téléchargement silencieux.
        let presenter = TestPresenter()
        let reopened = try world.makeUpdater()
        reopened.presenter = presenter
        reopened.start()
        try await world.wait { world.server.requests(to: world.checkPath).count == 2 && !reopened.isBusy }
        #expect(reopened.phase == .failed(.installationAtQuit(message)))
        #expect(presenter.presentations == 1)
        #expect(world.server.requests(to: publication.downloadPath).count == 1)

        // L'utilisateur réessaie lui-même : l'échec est oublié, et l'installation passe.
        await reopened.installAndRelaunch().value
        #expect(try FakeApps.version(of: world.installedApp) == "1.1.0")
        #expect(world.defaults.object(forKey: Updater.Keys.quitInstallFailure) == nil)
        #expect(world.relauncher.relaunched.count == 1)
    }

    @Test("Une préparation de plus d'un jour est refaite depuis Caches, sans nouveau téléchargement")
    func stalePreparationIsRedone() async throws {
        defer { world.cleanUp() }
        let publication = try world.publish(version: "1.1.0")
        let updater = try manualUpdater()
        updater.start()
        await updater.checkNow().value
        await updater.download().value
        let data = try #require(world.defaults.data(forKey: Updater.Keys.preparedUpdate))
        let json = try #require(try JSONSerialization.jsonObject(with: data) as? [String: Any])
        let prepared = try #require(json["prepared"] as? [String: Any])
        let path = try #require(prepared["workDirectory"] as? String)
        let firstWorkDirectory = try #require(URL(string: path))

        // Relancée plus d'un jour après : la préparation n'est pas reprise, l'archive resservira.
        world.clock.advance(by: 25 * 3_600)
        let later = try manualUpdater()
        later.start()
        #expect(later.phase == .idle)
        #expect(!exists(firstWorkDirectory))
        await later.checkNow().value
        await later.download().value
        #expect(later.phase == .ready)
        #expect(world.server.requests(to: publication.downloadPath).count == 1)

        // Ouverte plus d'un jour : la vérification suivante la refait aussi.
        world.clock.advance(by: 25 * 3_600)
        await later.checkNow().value
        #expect(later.phase == .available)
        await later.installAndRelaunch().value
        #expect(try FakeApps.version(of: world.installedApp) == "1.1.0")
        #expect(world.server.requests(to: publication.downloadPath).count == 1)
    }

    @Test("Intervalle ramené entre une heure et trente jours ; une valeur non finie vaut un jour")
    func boundedInterval() async throws {
        defer { world.cleanUp() }
        world.withdrawAll()
        world.clock.allow(1)
        let updater = try world.makeUpdater()
        updater.checkInterval = 1e30
        updater.start()
        try await world.wait { world.clock.sleeps.count == 1 }
        #expect(world.clock.sleeps == [.seconds(30 * 86_400)])

        for (value, expected) in [(Double.infinity, 86_400.0), (.nan, 86_400), (60, 3_600)] {
            let clock = TestClock()
            clock.allow(1)
            let other = try world.makeUpdater(clock: clock)
            other.checkInterval = value
            other.start()
            try await world.wait { clock.sleeps.count == 1 }
            #expect(clock.sleeps == [.seconds(expected)], "intervalle \(value)")
            // Sa vérification automatique s'arrête avec lui, à la fin de l'itération.
        }
    }

    @Test("Revenu au canal stable, l'app ne propose plus une bêta déjà trouvée")
    func backToStable() async throws {
        defer { world.cleanUp() }
        world.defaults.set("beta", forKey: Updater.Keys.channel)
        try world.publish(version: "1.1.0-beta.1", channel: .beta)
        let updater = try manualUpdater()
        updater.start()
        await updater.checkNow().value
        #expect(updater.availableUpdate?.version.description == "1.1.0-beta.1")
        #expect(world.server.requests.last?.url?.query()?.contains("channel=beta") == true)
        await updater.download().value
        #expect(updater.phase == .ready)

        updater.channel = .stable
        #expect(updater.phase == .idle)
        #expect(updater.availableUpdate == nil)
        #expect(world.defaults.object(forKey: Updater.Keys.preparedUpdate) == nil)
    }

    @Test("Canal changé pendant une vérification : la réponse de l'ancien canal est écartée, la vérification refaite")
    func channelChangedDuringCheck() async throws {
        defer { world.cleanUp() }
        world.defaults.set("beta", forKey: Updater.Keys.channel)
        let beta = try world.publish(version: "1.1.0-beta.1", channel: .beta)
        let offer = FakeServer.Response(
            status: 200, headers: ["Content-Type": "application/json"],
            body: try JSONSerialization.data(withJSONObject: beta.json)
        )
        // Le canal bêta reçoit la bêta, après un délai ; le canal stable n'a rien de plus récent.
        world.server.route(world.checkPath) { request in
            guard request.url?.query()?.contains("channel=beta") == true else { return .noContent }
            Thread.sleep(forTimeInterval: 0.3)
            return offer
        }
        let updater = try manualUpdater()
        updater.start()
        let check = updater.checkNow()
        try await world.wait { world.server.requests(to: world.checkPath).count == 1 }
        updater.channel = .stable
        await check.value

        #expect(updater.phase == .upToDate)
        #expect(updater.availableUpdate == nil)
        let queries = world.server.requests(to: world.checkPath).map { $0.url?.query() ?? "" }
        #expect(queries.count == 2)
        #expect(queries.last?.contains("channel=stable") == true)
    }

    @Test("Canal revenu à stable pendant une vérification qui échoue : la bêta déjà trouvée est oubliée, réessayer n'installe rien")
    func channelChangedDuringFailingCheck() async throws {
        defer { world.cleanUp() }
        world.defaults.set("beta", forKey: Updater.Keys.channel)
        let beta = try world.publish(version: "1.1.0-beta.1", channel: .beta)
        let updater = try manualUpdater()
        updater.start()
        await updater.checkNow().value
        #expect(updater.availableUpdate?.channel == .beta)

        // La vérification suivante reste en vol sur le canal bêta, puis échoue.
        world.server.route(world.checkPath) { request in
            if request.url?.query()?.contains("channel=beta") == true {
                Thread.sleep(forTimeInterval: 0.3)
            }
            return FakeServer.Response(status: 503)
        }
        let check = updater.checkNow()
        try await world.wait { world.server.requests(to: world.checkPath).count == 2 }
        updater.channel = .stable
        await check.value

        #expect(updater.phase == .failed(.server(status: 503, message: nil)))
        #expect(updater.availableUpdate == nil)
        // Il ne reste rien à installer : réessayer vérifie de nouveau.
        await updater.retry().value
        #expect(world.server.requests(to: world.checkPath).count == 3)
        #expect(world.server.requests(to: beta.downloadPath).isEmpty)
        #expect(world.relauncher.relaunched.isEmpty)
    }

    @Test("Progression du téléchargement suivie dans la phase")
    func downloadProgress() async throws {
        defer { world.cleanUp() }
        try world.publish(version: "1.1.0")
        let updater = try manualUpdater()
        updater.start()
        await updater.checkNow().value
        let observed = ObservedProgress()
        let observer = Task { @MainActor in
            while !Task.isCancelled {
                if case .downloading(let progress) = updater.phase, observed.values.last != progress {
                    observed.values.append(progress)
                }
                await Task.yield()
            }
        }
        await updater.download().value
        observer.cancel()
        #expect(updater.phase == .ready)
        #expect(observed.values.first == 0)
        #expect(observed.values == observed.values.sorted())
    }
}
