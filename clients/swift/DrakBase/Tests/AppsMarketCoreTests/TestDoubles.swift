@testable import AppsMarketCore
import AppsMarketCrypto
import AppsMarketTestSupport
import CryptoKit
import Foundation
import os

/// Remplace le contrôle de signature de code, et lui seul : les fausses apps ne sont signées par
/// aucun certificat, pour que les tests ne dépendent ni d'un Mac ni de son trousseau. Un bundle y
/// est « signé » par le certificat nommé dans son `Info.plist` (`TestSigningCertificate`) ; sans ce
/// nom, il est « ad hoc ».
struct SimulatedCodeSignature: CodeSignaturePolicy {
    static let infoKey = "TestSigningCertificate"

    func checkInstalledApp(at url: URL) throws(InstallationError) {
        guard Self.certificate(of: url) != nil else { throw .unavailable(.adHocSignature) }
    }

    /// L'exigence désignée simulée nomme le certificat de l'app installée : `certificate <nom>`.
    func designatedRequirement(ofInstalledApp url: URL) throws(InstallationError) -> String {
        guard let certificate = Self.certificate(of: url) else { throw .unavailable(.adHocSignature) }
        return "certificate \(certificate)"
    }

    /// Comme l'exigence désignée : le même certificat que l'app installée, ou une exigence de
    /// confiance de la même forme.
    func checkCandidate(at url: URL, designatedRequirement: String, trustedRequirements: [String]) throws(InstallationError) {
        guard let candidate = Self.certificate(of: url) else {
            throw .codeSignature("le nouveau bundle n'est pas signé avec un certificat")
        }
        guard ([designatedRequirement] + trustedRequirements).contains("certificate \(candidate)") else {
            throw .codeSignature("certificat « \(candidate) » refusé")
        }
    }

    static func certificate(of app: URL) -> String? {
        (try? FakeApps.info(of: app))?[infoKey] as? String
    }
}

/// Signature simulée qui refuse le bundle à partir d'un certain contrôle, pour simuler un bundle
/// remplacé entre le contrôle final et l'échange.
final class ChangingCodeSignature: CodeSignaturePolicy {
    private let refusedFromCheck: Int
    private let onRefusal: @Sendable () -> Void
    private let checks = OSAllocatedUnfairLock(initialState: 0)

    /// - Parameters:
    ///   - refusedFromCheck: rang, à partir de 1, du premier contrôle de bundle refusé.
    ///   - onRefusal: appelé à chaque refus, juste avant de le rendre.
    init(refusedFromCheck: Int, onRefusal: @escaping @Sendable () -> Void = {}) {
        self.refusedFromCheck = refusedFromCheck
        self.onRefusal = onRefusal
    }

    var candidateChecks: Int {
        checks.withLock { $0 }
    }

    func checkInstalledApp(at url: URL) throws(InstallationError) {
        try SimulatedCodeSignature().checkInstalledApp(at: url)
    }

    func designatedRequirement(ofInstalledApp url: URL) throws(InstallationError) -> String {
        try SimulatedCodeSignature().designatedRequirement(ofInstalledApp: url)
    }

    func checkCandidate(at url: URL, designatedRequirement: String, trustedRequirements: [String]) throws(InstallationError) {
        let rank = checks.withLock { count -> Int in
            count += 1
            return count
        }
        guard rank < refusedFromCheck else {
            onRefusal()
            throw .codeSignature("bundle remplacé")
        }
        try SimulatedCodeSignature().checkCandidate(at: url, designatedRequirement: designatedRequirement, trustedRequirements: trustedRequirements)
    }
}

/// Relance remplacée : la vraie fermerait le processus des tests.
@MainActor
final class TestRelauncher: AppRelauncher {
    private(set) var relaunched: [URL] = []
    var failure: UpdaterError?

    func relaunch(_ app: URL) async throws(UpdaterError) {
        relaunched.append(app)
        if let failure { throw failure }
    }
}

/// Horloge des vérifications automatiques : chaque attente avance l'heure d'autant, sans attendre
/// vraiment. Au-delà de `allowedSleeps`, l'attente dure jusqu'à l'annulation.
final class TestClock: Sendable {
    private struct State {
        var now = Date(timeIntervalSince1970: 1_790_000_000)
        var sleeps: [Duration] = []
        var allowedSleeps = Int.max
        var reads = 0
    }

    private let state = OSAllocatedUnfairLock(initialState: State())

    var now: Date {
        state.withLock { state in
            state.reads += 1
            return state.now
        }
    }

    /// Nombre de lectures de l'heure : une boucle qui tourne sans pause la lit sans arrêt.
    var reads: Int {
        state.withLock { $0.reads }
    }

    var sleeps: [Duration] {
        state.withLock { $0.sleeps }
    }

    func allow(_ count: Int) {
        state.withLock { $0.allowedSleeps = count }
    }

    func advance(by interval: TimeInterval) {
        state.withLock { $0.now.addTimeInterval(interval) }
    }

    func sleep(_ duration: Duration) async throws {
        let allowed = state.withLock { state -> Bool in
            guard state.sleeps.count < state.allowedSleeps else { return false }
            state.sleeps.append(duration)
            state.now.addTimeInterval(Double(duration.components.seconds) + Double(duration.components.attoseconds) / 1e18)
            return true
        }
        if !allowed {
            try await Task.sleep(for: .seconds(3600))
        }
    }
}

/// Monde d'essai : une fausse app installée dans un dossier temporaire, un faux serveur qui publie
/// ses versions signées par une clé jetable, des préférences et des dossiers à part.
@MainActor
final class UpdateWorld {
    static let slug = "essai"
    static let bundleIdentifier = "fr.example.essai"
    static let certificate = "Camille Essai (essai)"

    let directory: URL
    let applications: URL
    let installedApp: URL
    let installedVersion: SemanticVersion
    let caches: URL
    let server = FakeServer()
    let signer = TestSigner()
    let suiteName = "DrakBaseTests.\(UUID().uuidString)"
    let defaults: UserDefaults
    let notifications = NotificationCenter()
    let relauncher = TestRelauncher()
    let clock = TestClock()
    private var publications = 0

    init(installedVersion: String = "1.0.0", certificate: String? = UpdateWorld.certificate) throws {
        directory = try FakeApps.temporaryDirectory()
        applications = directory.appending(path: "Applications", directoryHint: .isDirectory)
        caches = directory.appending(path: "Caches", directoryHint: .isDirectory)
        try FileManager.default.createDirectory(at: applications, withIntermediateDirectories: true)
        installedApp = try FakeApps.make(
            in: applications,
            bundleIdentifier: Self.bundleIdentifier,
            version: installedVersion,
            info: certificate.map { [SimulatedCodeSignature.infoKey: $0] } ?? [:]
        )
        self.installedVersion = try SemanticVersion.parseClient(installedVersion)
        guard let defaults = UserDefaults(suiteName: suiteName) else { throw WorldError.defaults }
        self.defaults = defaults
    }

    enum WorldError: Error {
        case defaults
    }

    /// À appeler à la fin du test : dossiers et préférences disparaissent.
    func cleanUp() {
        defaults.removePersistentDomain(forName: suiteName)
        try? FileManager.default.removeItem(at: directory)
    }

    var feed: UpdateFeed {
        get throws {
            try UpdateFeed(baseURL: server.baseURL.appending(path: "api/v1/update/\(Self.slug)"))
        }
    }

    var app: InstalledApp {
        InstalledApp(url: installedApp, bundleIdentifier: Self.bundleIdentifier, version: installedVersion)
    }

    var keyring: MinisignKeyring {
        get throws {
            MinisignKeyring(primary: try signer.publicKey)
        }
    }

    /// Chemin de la vérification que fait l'app installée.
    var checkPath: String {
        "/api/v1/update/\(Self.slug)/darwin/\(CPUArchitecture.current.rawValue)/\(installedVersion)"
    }

    func installer(
        trustedRequirements: [String] = [],
        systemVersion: OperatingSystemVersion = ProcessInfo.processInfo.operatingSystemVersion
    ) throws -> LocalInstaller {
        LocalInstaller(
            app: app,
            publicKeys: try keyring,
            trustedRequirements: trustedRequirements,
            codeSignature: SimulatedCodeSignature(),
            systemVersion: systemVersion
        )
    }

    func makeUpdater(
        installer: (any UpdateInstaller)? = nil, allowsAnonymousStatistics: Bool = true, clock: TestClock? = nil
    ) throws -> Updater {
        let configuration = try UpdaterConfiguration(feed: feed, publicKeys: keyring)
        let setup = Updater.Setup(configuration: configuration, app: app, installer: try installer ?? self.installer())
        let clock = clock ?? self.clock
        let environment = Updater.Environment(
            defaults: defaults,
            session: server.session(),
            cachesDirectory: caches,
            notificationCenter: notifications,
            relauncher: relauncher,
            now: { clock.now },
            sleep: { try await clock.sleep($0) }
        )
        return Updater(setup: .success(setup), allowsAnonymousStatistics: allowsAnonymousStatistics, environment: environment)
    }

    /// Version publiée : la fausse app, son archive, sa signature, et la réponse du serveur.
    struct Publication {
        let response: UpdateResponse
        let json: [String: Any]
        let archive: URL
        let downloadPath: String
        let bundle: URL
    }

    /// Construit une version, l'archive et la signe comme `drakbase release`, puis la sert : la
    /// vérification de l'app installée la propose, et son adresse de téléchargement rend l'archive.
    ///
    /// - Parameters:
    ///   - bundleVersion: `CFBundleShortVersionString` du bundle, `version` par défaut.
    ///   - signedVersion: version du commentaire de confiance, `version` par défaut.
    ///   - archiveItems: contenu de l'archive à la place du seul bundle, à partir de son dossier.
    @discardableResult
    func publish(
        version: String,
        channel: UpdateChannel = .stable,
        notes: String? = "- Correction du raccourci clavier",
        bundleIdentifier: String = UpdateWorld.bundleIdentifier,
        bundleVersion: String? = nil,
        signedVersion: String? = nil,
        certificate: String? = UpdateWorld.certificate,
        minimumSystemVersion: String? = "14.0",
        signer: TestSigner? = nil,
        archiveItems: ((URL) throws -> [URL])? = nil,
        serve: Bool = true
    ) throws -> Publication {
        publications += 1
        let build = directory.appending(path: "build-\(publications)", directoryHint: .isDirectory)
        try FileManager.default.createDirectory(at: build, withIntermediateDirectories: true)
        let bundle = try FakeApps.make(
            in: build,
            bundleIdentifier: bundleIdentifier,
            version: bundleVersion ?? version,
            minimumSystemVersion: minimumSystemVersion,
            info: certificate.map { [SimulatedCodeSignature.infoKey: $0] } ?? [:]
        )
        let dist = directory.appending(path: "dist-\(publications)", directoryHint: .isDirectory)
        try FileManager.default.createDirectory(at: dist, withIntermediateDirectories: true)
        let archive = dist.appending(path: "Essai-\(version).app.tar.gz", directoryHint: .notDirectory)
        try FakeApps.archive(try archiveItems?(build) ?? [bundle], to: archive)
        let data = try Data(contentsOf: archive)
        let signature = try (signer ?? self.signer).signature(for: data, version: signedVersion ?? version, fileName: archive.lastPathComponent)

        let id = String(format: "01JB8Z6T3KQ4N5V7W9X0Y1Z%03d", publications)
        let downloadPath = "/dl/\(id)/\(archive.lastPathComponent)"
        // La forme de la réponse est celle du jeu d'essai commun (`fixtures/update-response.json`) :
        // seules les valeurs propres à cette publication changent.
        var json = try Fixtures.completeUpdateResponse()
        json["version"] = version
        json["url"] = server.baseURL.appending(path: String(downloadPath.dropFirst())).absoluteString
        json["signature"] = signature
        json["size"] = data.count
        json["sha256"] = SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
        json["channel"] = channel.rawValue
        json["notes"] = notes
        json["min_os_version"] = minimumSystemVersion
        let body = try JSONSerialization.data(withJSONObject: json, options: [.sortedKeys])
        let response = try JSONDecoder().decode(UpdateResponse.self, from: body)
        if serve {
            server.route(checkPath, response: FakeServer.Response(status: 200, headers: ["Content-Type": "application/json"], body: body))
            server.route(downloadPath, response: FakeServer.Response(body: data, chunkSize: max(data.count / 8, 1), chunkDelay: 0.005))
        }
        return Publication(response: response, json: json, archive: archive, downloadPath: downloadPath, bundle: bundle)
    }

    /// Le serveur ne propose plus rien : 204.
    func withdrawAll() {
        server.route(checkPath, response: .noContent)
    }

    /// Attend qu'une condition devienne vraie, sans dépasser `timeout`.
    func wait(timeout: Duration = .seconds(10), until condition: () -> Bool) async throws {
        let clock = ContinuousClock()
        let deadline = clock.now + timeout
        while !condition() {
            guard clock.now < deadline else {
                throw WaitTimeout()
            }
            try await Task.sleep(for: .milliseconds(10))
        }
    }

    struct WaitTimeout: Error {}
}
