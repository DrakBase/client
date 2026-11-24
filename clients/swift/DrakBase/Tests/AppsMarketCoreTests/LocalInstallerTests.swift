@testable import AppsMarketCore
import AppsMarketCrypto
import AppsMarketTestSupport
import Foundation
import os
import Testing

/// Préparation, contrôles et échange sur de fausses apps dans un dossier temporaire. Seul le
/// contrôle de signature de code est simulé (`SimulatedCodeSignature`).
@Suite("Installation locale")
@MainActor
struct LocalInstallerTests {
    let world: UpdateWorld

    init() throws {
        world = try UpdateWorld()
    }

    func prepare(_ publication: UpdateWorld.Publication, installer: LocalInstaller? = nil) async throws -> PreparedUpdate {
        try await (installer ?? world.installer()).prepare(archive: publication.archive, for: publication.response)
    }

    /// Erreur d'installation levée par `body`, ou `nil`.
    func installationError(_ body: () async throws -> Void) async -> InstallationError? {
        do {
            try await body()
            return nil
        } catch let error as InstallationError {
            return error
        } catch {
            Issue.record("erreur inattendue : \(error)")
            return nil
        }
    }

    @Test("Préparation : dossier de travail sur le volume de l'app, un seul .app extrait, quarantaine retirée")
    func preparation() async throws {
        defer { world.cleanUp() }
        let publication = try world.publish(version: "1.1.0")
        try FakeApps.quarantine(publication.archive)
        let installer = try world.installer()
        #expect(installer.unavailability() == nil)

        let prepared = try await prepare(publication, installer: installer)
        #expect(prepared.version.description == "1.1.0")
        #expect(prepared.bundleURL.lastPathComponent == "Essai.app")
        #expect(BundleFiles.url(prepared.bundleURL, isInside: prepared.workDirectory))
        #expect(try FakeApps.version(of: prepared.bundleURL) == "1.1.0")
        let volume = { (url: URL) throws -> NSObject? in
            try url.resourceValues(forKeys: [.volumeIdentifierKey]).volumeIdentifier as? NSObject
        }
        #expect(try volume(prepared.workDirectory) == volume(world.installedApp))
        // La copie de l'archive ne sert plus après l'extraction.
        #expect(try FileManager.default.contentsOfDirectory(atPath: prepared.workDirectory.path(percentEncoded: false)) == ["extract"])

        // tar recopie la quarantaine de l'archive sur chaque fichier : elle est retirée partout.
        #expect(FakeApps.hasQuarantine(publication.archive))
        let enumerator = try #require(FileManager.default.enumerator(at: prepared.bundleURL, includingPropertiesForKeys: nil))
        var files = [prepared.bundleURL]
        while let file = enumerator.nextObject() as? URL {
            files.append(file)
        }
        #expect(files.count > 3)
        #expect(files.filter(FakeApps.hasQuarantine).isEmpty)

        // L'app installée n'a pas bougé.
        #expect(try FakeApps.version(of: world.installedApp) == "1.0.0")
        installer.discard(prepared)
        #expect(!FileManager.default.fileExists(atPath: prepared.workDirectory.path(percentEncoded: false)))
    }

    @Test("Échange : la nouvelle version prend la place de l'app, l'ancienne reste dans le dossier de travail")
    func exchange() async throws {
        defer { world.cleanUp() }
        let publication = try world.publish(version: "1.1.0")
        let installer = try world.installer()
        let prepared = try await prepare(publication, installer: installer)

        let installed = try installer.install(prepared)
        #expect(installed.version.description == "1.1.0")
        #expect(try FakeApps.version(of: world.installedApp) == "1.1.0")
        let previous = try #require(installed.previousBundle)
        #expect(previous == prepared.bundleURL)
        #expect(try FakeApps.version(of: previous) == "1.0.0")
        #expect(FileManager.default.fileExists(atPath: world.installedApp.appending(path: "Contents/MacOS/Essai").path(percentEncoded: false)))
    }

    @Test("L'archive doit contenir un seul dossier .app à sa racine")
    func archiveLayout() async throws {
        defer { world.cleanUp() }
        let layouts: [(String, (URL) throws -> [URL], String)] = [
            ("deux apps", { build in
                [try FakeApps.make(in: build, name: "Autre", version: "1.1.0"), build.appending(path: "Essai.app")]
            }, "trouvé Autre.app, Essai.app"),
            ("une app et un fichier", { build in
                try Data("bonjour".utf8).write(to: build.appending(path: "LISEZMOI.txt"))
                return [build.appending(path: "Essai.app"), build.appending(path: "LISEZMOI.txt")]
            }, "trouvé Essai.app, LISEZMOI.txt"),
            ("un fichier seul", { build in
                try Data("bonjour".utf8).write(to: build.appending(path: "LISEZMOI.txt"))
                return [build.appending(path: "LISEZMOI.txt")]
            }, "trouvé LISEZMOI.txt"),
            ("un lien vers l'app", { build in
                try FileManager.default.createSymbolicLink(
                    at: build.appending(path: "Lien.app"), withDestinationURL: build.appending(path: "Essai.app")
                )
                return [build.appending(path: "Lien.app")]
            }, "trouvé Lien.app"),
            ("un dossier qui n'est pas une app", { build in
                let folder = build.appending(path: "Essai", directoryHint: .isDirectory)
                try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
                try FileManager.default.moveItem(at: build.appending(path: "Essai.app"), to: folder.appending(path: "Essai.app"))
                return [folder]
            }, "trouvé Essai"),
        ]
        for (name, items, detail) in layouts {
            let publication = try world.publish(version: "1.1.0", archiveItems: items)
            let error = await installationError { _ = try await prepare(publication) }
            #expect(error == .archiveLayout(detail), "\(name)")
        }
    }

    @Test("La copie de l'archive est vérifiée : fichier modifié, autre clé, version signée différente ou absente")
    func archiveSignature() async throws {
        defer { world.cleanUp() }
        let tampered = try world.publish(version: "1.1.0")
        var data = try Data(contentsOf: tampered.archive)
        data[data.count / 2] ^= 1
        try data.write(to: tampered.archive)
        #expect(await installationError { _ = try await prepare(tampered) } == .signature(.invalid))

        let stranger = TestSigner()
        let foreign = try world.publish(version: "1.1.0", signer: stranger)
        let expected = try world.signer.publicKey.keyID
        let found = try stranger.publicKey.keyID
        #expect(await installationError { _ = try await prepare(foreign) } == .signature(.keyIDMismatch(expected: expected, found: found)))

        let mislabeled = try world.publish(version: "1.1.0", signedVersion: "1.2.0")
        #expect(await installationError { _ = try await prepare(mislabeled) }
            == .signature(.versionMismatch(signed: "1.2.0", expected: "1.1.0")))

        let unlabeled = try world.publish(version: "1.1.0")
        let unsigned = try world.signer.signature(forFileAt: unlabeled.archive, version: nil)
        var json = unlabeled.json
        json["signature"] = unsigned
        let response = try JSONDecoder().decode(UpdateResponse.self, from: JSONSerialization.data(withJSONObject: json))
        #expect(await installationError { _ = try await world.installer().prepare(archive: unlabeled.archive, for: response) }
            == .signature(.missingVersion))
    }

    @Test("Le bundle : même identifiant, version annoncée et signée, plus récente, macOS compatible")
    func bundleChecks() async throws {
        defer { world.cleanUp() }
        let otherApp = try world.publish(version: "1.1.0", bundleIdentifier: "fr.example.autre")
        #expect(await installationError { _ = try await prepare(otherApp) }
            == .bundleIdentifierMismatch(expected: "fr.example.essai", found: "fr.example.autre"))

        let otherVersion = try world.publish(version: "1.1.0", bundleVersion: "1.1.1")
        #expect(await installationError { _ = try await prepare(otherVersion) }
            == .bundleVersionMismatch(found: "1.1.1", expected: "1.1.0"))

        let sameVersion = try world.publish(version: "1.0.0")
        #expect(await installationError { _ = try await prepare(sameVersion) } == .notNewer(found: "1.0.0", installed: "1.0.0"))
        let older = try world.publish(version: "0.9.0")
        #expect(await installationError { _ = try await prepare(older) } == .notNewer(found: "0.9.0", installed: "1.0.0"))

        let newSystem = try world.publish(version: "1.1.0", minimumSystemVersion: "26.1")
        let sonoma = try world.installer(systemVersion: OperatingSystemVersion(majorVersion: 14, minorVersion: 6, patchVersion: 1))
        #expect(await installationError { _ = try await prepare(newSystem, installer: sonoma) }
            == .requiresNewerSystem(required: "26.1", current: "14.6.1"))
        let tahoe = try world.installer(systemVersion: OperatingSystemVersion(majorVersion: 26, minorVersion: 1, patchVersion: 0))
        #expect(try await prepare(newSystem, installer: tahoe).version.description == "1.1.0")

        // Lecture souple, comme le serveur : les nombres de tête comptent.
        let betaSystem = try world.publish(version: "1.1.0", minimumSystemVersion: "26.1-beta")
        #expect(await installationError { _ = try await prepare(betaSystem, installer: sonoma) }
            == .requiresNewerSystem(required: "26.1-beta", current: "14.6.1"))
        #expect(try await prepare(betaSystem, installer: tahoe).version.description == "1.1.0")

        // Un minimum vraiment illisible : le serveur ne l'écarte pas, l'installeur le refuse.
        let unreadableSystem = try world.publish(version: "1.1.0", minimumSystemVersion: "quatorze")
        #expect(await installationError { _ = try await prepare(unreadableSystem) }
            == .unreadableBundle("LSMinimumSystemVersion « quatorze » illisible"))
        let noMinimum = try world.publish(version: "1.1.0", minimumSystemVersion: nil)
        #expect(try await prepare(noMinimum).version.description == "1.1.0")
    }

    @Test("Pas de retour arrière non plus par rapport à l'app sur le disque, plus récente que celle qui tourne")
    func notNewerThanDisk() async throws {
        defer { world.cleanUp() }
        // Un échange a réussi, mais l'app n'a pas été relancée : elle tourne en 1.0.0, la 1.2.0 est installée.
        var info = try FakeApps.info(of: world.installedApp)
        info["CFBundleShortVersionString"] = "1.2.0"
        try FakeApps.writeInfo(info, to: world.installedApp)
        let older = try world.publish(version: "1.1.0")
        #expect(await installationError { _ = try await prepare(older) } == .notNewer(found: "1.1.0", installed: "1.2.0"))
        let newer = try world.publish(version: "1.3.0")
        #expect(try await prepare(newer).version.description == "1.3.0")
    }

    @Test("Un bundle remplacé entre le contrôle final et l'échange est refusé, et l'app remise en place")
    func replacedDuringExchange() async throws {
        defer { world.cleanUp() }
        // Contrôles du bundle : à la préparation, au contrôle final, puis après l'échange, refusé.
        let policy = ChangingCodeSignature(refusedFromCheck: 3)
        let installer = LocalInstaller(
            app: world.app, publicKeys: try world.keyring, codeSignature: policy,
            systemVersion: ProcessInfo.processInfo.operatingSystemVersion
        )
        let prepared = try await prepare(try world.publish(version: "1.1.0"), installer: installer)
        #expect(await installationError { _ = try installer.install(prepared) } == .codeSignature("bundle remplacé"))
        #expect(policy.candidateChecks == 3)
        #expect(try FakeApps.version(of: world.installedApp) == "1.0.0")
        #expect(try FakeApps.version(of: prepared.bundleURL) == "1.1.0")
    }

    @Test("Sans échange atomique, un bundle refusé après l'échange rend aussi sa place à l'app, et rien n'est perdu")
    func replacedDuringExchangeWithoutSwap() async throws {
        defer { world.cleanUp() }
        let installer = try installer(codeSignature: ChangingCodeSignature(refusedFromCheck: 3), exchangesAtomically: false)
        let prepared = try await prepare(try world.publish(version: "1.1.0"), installer: installer)
        #expect(await installationError { _ = try installer.install(prepared) } == .codeSignature("bundle remplacé"))
        #expect(try FakeApps.version(of: world.installedApp) == "1.0.0")
        // Le bundle refusé est retourné dans le dossier de travail, que l'abandon supprime.
        #expect(try FakeApps.version(of: prepared.bundleURL) == "1.1.0")
        installer.discard(prepared)
        #expect(!FileManager.default.fileExists(atPath: prepared.workDirectory.path(percentEncoded: false)))
        #expect(try FakeApps.version(of: world.installedApp) == "1.0.0")
    }

    @Test("Si l'ancienne version ne peut pas reprendre sa place, elle est gardée : l'abandon ne supprime rien")
    func restorationFailure() async throws {
        defer { world.cleanUp() }
        let applications = world.applications.path(percentEncoded: false)
        let setMode: @Sendable (Int) -> Void = { mode in
            try? FileManager.default.setAttributes([.posixPermissions: mode], ofItemAtPath: applications)
        }
        defer { setMode(0o755) }
        // Quand le bundle est refusé, après l'échange, le dossier de l'app n'est plus modifiable :
        // plus aucun renommage n'y passe.
        let installer = try installer(codeSignature: ChangingCodeSignature(refusedFromCheck: 3) { setMode(0o555) })
        let prepared = try await prepare(try world.publish(version: "1.1.0"), installer: installer)
        defer { try? FileManager.default.removeItem(at: prepared.workDirectory) }
        let failure = await installationError { _ = try installer.install(prepared) }
        setMode(0o755)

        guard case .exchange(let detail) = failure else {
            Issue.record("erreur inattendue : \(String(describing: failure))")
            return
        }
        #expect(detail.hasPrefix("bundle remplacé ; l'ancienne version n'a pas repris sa place"))
        #expect(detail.contains("previous/Essai.app"))
        let kept = prepared.workDirectory.appending(path: "previous/Essai.app")
        #expect(try FakeApps.version(of: kept) == "1.0.0")
        #expect(try FakeApps.version(of: world.installedApp) == "1.1.0")

        // La seule copie de l'ancienne version est dans le dossier de travail : il reste.
        installer.discard(prepared)
        #expect(try FakeApps.version(of: kept) == "1.0.0")
    }

    @Test("Restée à la place du bundle préparé, faute de pouvoir être déplacée, l'ancienne version garde aussi son dossier de travail")
    func restorationFailureWithoutBackup() async throws {
        defer { world.cleanUp() }
        // Quand le bundle est refusé, après l'échange, ni le dossier de l'app ni le dossier de travail
        // ne sont plus modifiables : pas d'échange de retour, ni de dossier de sauvegarde.
        let applications = world.applications.path(percentEncoded: false)
        let workDirectory = OSAllocatedUnfairLock<String?>(initialState: nil)
        let setMode: @Sendable (Int) -> Void = { mode in
            for path in [applications, workDirectory.withLock { $0 }].compactMap({ $0 }) {
                try? FileManager.default.setAttributes([.posixPermissions: mode], ofItemAtPath: path)
            }
        }
        defer { setMode(0o755) }
        let installer = try installer(codeSignature: ChangingCodeSignature(refusedFromCheck: 3) { setMode(0o555) })
        let prepared = try await prepare(try world.publish(version: "1.1.0"), installer: installer)
        defer { try? FileManager.default.removeItem(at: prepared.workDirectory) }
        workDirectory.withLock { $0 = prepared.workDirectory.path(percentEncoded: false) }
        let failure = await installationError { _ = try installer.install(prepared) }
        setMode(0o755)

        guard case .exchange(let detail) = failure else {
            Issue.record("erreur inattendue : \(String(describing: failure))")
            return
        }
        // L'erreur désigne l'ancienne version là où l'échange l'a laissée.
        #expect(detail.hasPrefix("bundle remplacé ; l'ancienne version n'a pas repris sa place"))
        #expect(detail.contains("extract/Essai.app"))
        #expect(try FakeApps.version(of: prepared.bundleURL) == "1.0.0")
        #expect(try FakeApps.version(of: world.installedApp) == "1.1.0")

        // Le dossier de sauvegarde est vide, mais la place du bundle préparé n'a pas la version
        // préparée : le dossier de travail reste.
        installer.discard(prepared)
        #expect(try FakeApps.version(of: prepared.bundleURL) == "1.0.0")
    }

    /// Installeur dont le contrôle de signature de code et le mode d'échange sont choisis.
    func installer(codeSignature: any CodeSignaturePolicy, exchangesAtomically: Bool = true) throws -> LocalInstaller {
        var installer = LocalInstaller(
            app: world.app, publicKeys: try world.keyring, codeSignature: codeSignature,
            systemVersion: ProcessInfo.processInfo.operatingSystemVersion
        )
        installer.exchangesAtomically = exchangesAtomically
        return installer
    }

    @Test("Un bundle en « 1.2 » est signé « 1.2.0 » : la version est normalisée avant d'être comparée")
    func normalizedBundleVersion() async throws {
        defer { world.cleanUp() }
        let publication = try world.publish(version: "1.2.0", bundleVersion: "1.2")
        let prepared = try await prepare(publication)
        #expect(prepared.version.description == "1.2.0")
        #expect(try FakeApps.version(of: prepared.bundleURL) == "1.2")
    }

    @Test("Signature de code : autre certificat refusé, sauf exigence de confiance ; bundle non signé refusé")
    func codeSignature() async throws {
        defer { world.cleanUp() }
        let otherCertificate = try world.publish(version: "1.1.0", certificate: "Nouveau certificat")
        #expect(await installationError { _ = try await prepare(otherCertificate) }
            == .codeSignature("certificat « Nouveau certificat » refusé"))
        let trusting = try world.installer(trustedRequirements: ["certificate Nouveau certificat"])
        #expect(try await prepare(otherCertificate, installer: trusting).version.description == "1.1.0")

        let unsigned = try world.publish(version: "1.1.0", certificate: nil)
        #expect(await installationError { _ = try await prepare(unsigned) }
            == .codeSignature("le nouveau bundle n'est pas signé avec un certificat"))
    }

    @Test("App installée signée ad hoc : mise à jour indisponible")
    func adHocInstalledApp() async throws {
        let adHoc = try UpdateWorld(certificate: nil)
        defer { adHoc.cleanUp() }
        let installer = try adHoc.installer()
        #expect(installer.unavailability() == .adHocSignature)
        let publication = try adHoc.publish(version: "1.1.0")
        let error = await installationError {
            _ = try await installer.prepare(archive: publication.archive, for: publication.response)
        }
        #expect(error == .unavailable(.adHocSignature))
    }

    @Test("App transloquée, ou dans un dossier non modifiable : mise à jour indisponible")
    func unavailableLocations() async throws {
        defer { world.cleanUp() }
        let translocated = world.directory.appending(path: "AppTranslocation/0F8FAD5B/d", directoryHint: .isDirectory)
        try FileManager.default.createDirectory(at: translocated, withIntermediateDirectories: true)
        let translocatedApp = try FakeApps.make(in: translocated, version: "1.0.0", info: [SimulatedCodeSignature.infoKey: UpdateWorld.certificate])
        let translocatedInstaller = LocalInstaller(
            app: InstalledApp(url: translocatedApp, bundleIdentifier: UpdateWorld.bundleIdentifier, version: world.installedVersion),
            publicKeys: try world.keyring,
            codeSignature: SimulatedCodeSignature(),
            systemVersion: ProcessInfo.processInfo.operatingSystemVersion
        )
        #expect(translocatedInstaller.unavailability() == .translocated)

        let installer = try world.installer()
        let publication = try world.publish(version: "1.1.0")
        let prepared = try await prepare(publication, installer: installer)

        let folder = world.applications.path(percentEncoded: false)
        try FileManager.default.setAttributes([.posixPermissions: 0o555], ofItemAtPath: folder)
        defer { try? FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: folder) }
        #expect(installer.unavailability() == .readOnlyLocation(world.applications.path(percentEncoded: false)))
        #expect(await installationError { _ = try installer.install(prepared) }
            == .unavailable(.readOnlyLocation(world.applications.path(percentEncoded: false))))
        #expect(try FakeApps.version(of: world.installedApp) == "1.0.0")

        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: folder)
        let bundle = world.installedApp.path(percentEncoded: false)
        try FileManager.default.setAttributes([.posixPermissions: 0o555], ofItemAtPath: bundle)
        defer { try? FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: bundle) }
        #expect(installer.unavailability() == .readOnlyLocation(world.applications.path(percentEncoded: false)))
    }

    @Test("Contrôle final : un bundle préparé modifié depuis n'est pas installé")
    func finalCheck() async throws {
        defer { world.cleanUp() }
        let installer = try world.installer()
        let prepared = try await prepare(try world.publish(version: "1.1.0"), installer: installer)
        var info = try FakeApps.info(of: prepared.bundleURL)
        info["CFBundleShortVersionString"] = "1.1.1"
        try FakeApps.writeInfo(info, to: prepared.bundleURL)
        #expect(await installationError { _ = try installer.install(prepared) } == .bundleVersionMismatch(found: "1.1.1", expected: "1.1.0"))

        info["CFBundleShortVersionString"] = "1.1.0"
        info[SimulatedCodeSignature.infoKey] = "Quelqu'un d'autre"
        try FakeApps.writeInfo(info, to: prepared.bundleURL)
        #expect(await installationError { _ = try installer.install(prepared) } == .codeSignature("certificat « Quelqu'un d'autre » refusé"))
        #expect(try FakeApps.version(of: world.installedApp) == "1.0.0")

        // Une préparation dont le bundle n'est pas dans son dossier de travail est refusée.
        let elsewhere = PreparedUpdate(version: prepared.version, bundleURL: world.installedApp, workDirectory: prepared.workDirectory)
        let error = await installationError { _ = try installer.install(elsewhere) }
        guard case .fileSystem = error else {
            Issue.record("erreur inattendue : \(String(describing: error))")
            return
        }
    }

    @Test("Abandon d'une préparation : son dossier disparaît, sauf si l'app manque à sa place")
    func discard() async throws {
        defer { world.cleanUp() }
        let installer = try world.installer()
        let prepared = try await prepare(try world.publish(version: "1.1.0"), installer: installer)
        let exists = { FileManager.default.fileExists(atPath: prepared.workDirectory.path(percentEncoded: false)) }

        // Un échange interrompu a pu laisser l'app dans le dossier de travail : il reste.
        let elsewhere = world.directory.appending(path: "Ailleurs.app", directoryHint: .isDirectory)
        try FileManager.default.moveItem(at: world.installedApp, to: elsewhere)
        installer.discard(prepared)
        #expect(exists())

        try FileManager.default.moveItem(at: elsewhere, to: world.installedApp)
        installer.discard(prepared)
        #expect(!exists())
        // Une préparation incohérente ne fait rien supprimer.
        installer.discard(PreparedUpdate(version: prepared.version, bundleURL: world.installedApp, workDirectory: world.applications))
        #expect(FileManager.default.fileExists(atPath: world.installedApp.path(percentEncoded: false)))
    }

    @Test("Échange atomique, et repli par renommages qui remet l'app en place s'il échoue")
    func exchangePrimitives() throws {
        defer { world.cleanUp() }
        let staging = world.directory.appending(path: "staging", directoryHint: .isDirectory)
        try FileManager.default.createDirectory(at: staging, withIntermediateDirectories: true)
        let candidate = try FakeApps.make(in: staging, version: "1.1.0")

        let previous = try BundleFiles.exchange(candidate: candidate, installed: world.installedApp, backupDirectory: staging.appending(path: "previous"))
        #expect(previous == candidate)
        #expect(try FakeApps.version(of: world.installedApp) == "1.1.0")
        #expect(try FakeApps.version(of: previous) == "1.0.0")

        // Repli : l'app installée part de côté, puis la nouvelle version prend sa place.
        let next = try FakeApps.make(in: world.directory.appending(path: "staging-2"), version: "1.2.0")
        let backup = try BundleFiles.exchangeWithBackup(
            candidate: next, installed: world.installedApp, backupDirectory: world.directory.appending(path: "backup")
        )
        #expect(try FakeApps.version(of: world.installedApp) == "1.2.0")
        #expect(try FakeApps.version(of: backup) == "1.1.0")
        #expect(!FileManager.default.fileExists(atPath: next.path(percentEncoded: false)))

        // Si la nouvelle version ne peut pas prendre la place, l'app installée y revient.
        let missing = world.directory.appending(path: "absent/Essai.app")
        #expect(throws: InstallationError.self) {
            try BundleFiles.exchangeWithBackup(candidate: missing, installed: world.installedApp, backupDirectory: world.directory.appending(path: "backup-2"))
        }
        #expect(try FakeApps.version(of: world.installedApp) == "1.2.0")
        let failure = #expect(throws: InstallationError.self) {
            try BundleFiles.exchange(candidate: missing, installed: world.installedApp, backupDirectory: world.directory.appending(path: "backup-3"))
        }
        guard case .exchange(let detail) = failure else {
            Issue.record("erreur inattendue : \(String(describing: failure))")
            return
        }
        #expect(detail.hasPrefix("échange atomique refusé"))
        #expect(try FakeApps.version(of: world.installedApp) == "1.2.0")
    }
}
