import AppsMarketCrypto
import CoreServices
import Foundation
import OSLog

private let log = Logger(subsystem: "DrakBase", category: "installer")

/// Installation dans le processus de l'app, pour une app hors sandbox (`docs/PLAN.md`,
/// « Déroulé », étapes 4 à 6).
public struct LocalInstaller: UpdateInstaller {
    let app: InstalledApp
    let publicKeys: MinisignKeyring
    let trustedRequirements: [String]
    let codeSignature: any CodeSignaturePolicy
    let systemVersion: SystemVersion
    /// Faux dans les tests seulement : l'échange passe alors par le repli en deux renommages, comme
    /// sur un volume sans échange atomique.
    var exchangesAtomically = true

    /// Installeur de l'app qui tourne, avec les clés et les exigences de son `Info.plist`.
    public init(app: InstalledApp, configuration: UpdaterConfiguration) {
        self.init(
            app: app,
            publicKeys: configuration.publicKeys,
            trustedRequirements: configuration.trustedRequirements,
            codeSignature: SecurityCodeSignaturePolicy(),
            systemVersion: ProcessInfo.processInfo.operatingSystemVersion
        )
    }

    /// Réservé aux tests du paquet (`@testable import`) : seul le contrôle de signature de code,
    /// et la version de macOS, y sont remplacés.
    init(
        app: InstalledApp,
        publicKeys: MinisignKeyring,
        trustedRequirements: [String] = [],
        codeSignature: any CodeSignaturePolicy,
        systemVersion: OperatingSystemVersion
    ) {
        self.app = app
        self.publicKeys = publicKeys
        self.trustedRequirements = trustedRequirements
        self.codeSignature = codeSignature
        self.systemVersion = SystemVersion(systemVersion)
    }

    public func unavailability() -> UpdateUnavailability? {
        if app.url.path(percentEncoded: false).contains("/AppTranslocation/") {
            return .translocated
        }
        let folder = app.url.deletingLastPathComponent()
        // Renommer le bundle d'un dossier à l'autre exige d'écrire dans les deux.
        guard BundleFiles.isWritable(folder), BundleFiles.isWritable(app.url) else {
            return .readOnlyLocation(folder.path(percentEncoded: false))
        }
        do {
            try codeSignature.checkInstalledApp(at: app.url)
        } catch {
            if case .unavailable(let reason) = error { return reason }
            return .unsigned
        }
        return nil
    }

    @concurrent
    public func prepare(archive: URL, for update: UpdateResponse) async throws -> PreparedUpdate {
        if let reason = unavailability() { throw InstallationError.unavailable(reason) }
        let work = try workDirectory()
        do {
            // La copie est vérifiée à son tour : elle seule sera extraite.
            let copy = work.appending(path: "archive.app.tar.gz", directoryHint: .notDirectory)
            try FileManager.default.copyItem(at: archive, to: copy)
            let signature = try MinisignSignature(tauri: update.signature)
            try await publicKeys.verifyFile(at: copy, signature: signature).requireVersion(update.version)

            let extracted = work.appending(path: "extract", directoryHint: .isDirectory)
            try FileManager.default.createDirectory(at: extracted, withIntermediateDirectories: false)
            try BundleFiles.extract(copy, into: extracted)
            try FileManager.default.removeItem(at: copy)

            let candidate = try BundleFiles.singleApp(in: extracted)
            try inspect(candidate, expectedVersion: update.version)
            try BundleFiles.removeQuarantine(from: candidate)
            log.notice("préparée : \(update.version, privacy: .public) dans \(work.path(percentEncoded: false), privacy: .public)")
            return PreparedUpdate(version: update.version, bundleURL: candidate, workDirectory: work)
        } catch {
            try? FileManager.default.removeItem(at: work)
            switch error {
            case let error as InstallationError: throw error
            case let error as SignatureError: throw InstallationError.signature(error)
            case let error as CancellationError: throw error
            default: throw InstallationError.fileSystem(error.localizedDescription)
            }
        }
    }

    public func install(_ prepared: PreparedUpdate) throws -> InstalledUpdate {
        if let reason = unavailability() { throw InstallationError.unavailable(reason) }
        guard BundleFiles.url(prepared.bundleURL, isInside: prepared.workDirectory),
              !BundleFiles.url(app.url, isInside: prepared.workDirectory)
        else {
            throw InstallationError.fileSystem("préparation incohérente : \(prepared.bundleURL.path(percentEncoded: false))")
        }
        // Contrôle final : la préparation peut dater d'un lancement précédent, et son dossier avoir
        // changé depuis.
        let requirement = try inspect(prepared.bundleURL, expectedVersion: prepared.version)
        let backup = backupDirectory(of: prepared)
        let previous = try exchangesAtomically
            ? BundleFiles.exchange(candidate: prepared.bundleURL, installed: app.url, backupDirectory: backup)
            : BundleFiles.exchangeWithBackup(candidate: prepared.bundleURL, installed: app.url, backupDirectory: backup)
        // Le dossier de travail reste modifiable par d'autres processus : le bundle a pu y être remplacé
        // entre le contrôle final et l'échange. Il est donc revérifié à sa place définitive, avec
        // l'exigence lue avant l'échange, et l'ancienne version reprend sa place s'il ne passe plus.
        do {
            try checkInstalledBundle(expectedVersion: prepared.version, designatedRequirement: requirement)
        } catch {
            try restore(previous, of: prepared, refusal: error)
            throw error
        }
        let status = LSRegisterURL(app.url as CFURL, true)
        if status != noErr {
            log.error("LSRegisterURL : \(status)")
        }
        log.notice("installée : \(prepared.version, privacy: .public), ancienne version dans \(previous.path(percentEncoded: false), privacy: .public)")
        return InstalledUpdate(version: prepared.version, previousBundle: previous)
    }

    public func discard(_ prepared: PreparedUpdate) {
        // Seul un dossier de travail qui contient bien la préparation, et pas l'app, est supprimé. Si
        // l'app manque à sa place, un échange interrompu a pu la laisser dans ce dossier : on n'y
        // touche pas. De même s'il peut garder une ancienne version mise de côté, qui n'a pas repris
        // sa place : c'est peut-être la seule copie de l'app.
        guard BundleFiles.url(prepared.bundleURL, isInside: prepared.workDirectory),
              !BundleFiles.url(app.url, isInside: prepared.workDirectory),
              prepared.workDirectory.resolvingSymlinksInPath().pathComponents.count > 3,
              FileManager.default.fileExists(atPath: app.url.path(percentEncoded: false)),
              !mayHoldPreviousVersion(prepared)
        else {
            log.error("dossier de travail gardé : \(prepared.workDirectory.path(percentEncoded: false), privacy: .public)")
            return
        }
        try? FileManager.default.removeItem(at: prepared.workDirectory)
    }

    /// Vrai si le dossier de travail peut contenir l'ancienne version, mise de côté par un échange :
    /// le dossier de sauvegarde n'est pas vide, ou la place du bundle préparé est occupée par autre
    /// chose que la version préparée. Un échange atomique y laisse l'ancienne version, et elle y
    /// reste si rien n'a pu l'en déplacer.
    private func mayHoldPreviousVersion(_ prepared: PreparedUpdate) -> Bool {
        guard BundleFiles.isEmptyOrMissing(backupDirectory(of: prepared)) else { return true }
        guard FileManager.default.fileExists(atPath: prepared.bundleURL.path(percentEncoded: false)) else { return false }
        let version = (try? BundleInfo(at: prepared.bundleURL)).flatMap { try? SemanticVersion.parseClient($0.shortVersion) }
        return version != prepared.version
    }

    // MARK: - Contrôles

    /// Contrôles du nouveau bundle (étape 4) : identifiant, version, version de macOS, signature de
    /// code. L'app installée signée ad hoc est refusée avant tout contrôle de signature. Rend
    /// l'exigence désignée de l'app installée.
    @discardableResult
    func inspect(_ candidate: URL, expectedVersion: SemanticVersion) throws(InstallationError) -> String {
        let info = try BundleInfo(at: candidate)
        guard info.identifier == app.bundleIdentifier else {
            throw .bundleIdentifierMismatch(expected: app.bundleIdentifier, found: info.identifier)
        }
        // « 1.2 » dans le bundle est signé « 1.2.0 » : la version est normalisée comme celle d'un client.
        let version: SemanticVersion
        do {
            version = try SemanticVersion.parseClient(info.shortVersion)
        } catch {
            throw .unreadableBundle("CFBundleShortVersionString « \(info.shortVersion) » illisible")
        }
        guard version == expectedVersion else {
            throw .bundleVersionMismatch(found: version.description, expected: expectedVersion.description)
        }
        // L'app sur le disque peut être plus récente que celle qui tourne, si un échange a réussi sans
        // la relance : pas de retour arrière par rapport à l'une ou à l'autre.
        let onDisk = (try? BundleInfo(at: app.url)).flatMap { try? SemanticVersion.parseClient($0.shortVersion) }
        let installed = max(app.version, onDisk ?? app.version)
        guard version > installed else {
            throw .notNewer(found: version.description, installed: installed.description)
        }
        if let minimum = info.minimumSystemVersion {
            guard let required = SystemVersion(minimum) else {
                throw .unreadableBundle("LSMinimumSystemVersion « \(minimum) » illisible")
            }
            guard required <= systemVersion else {
                throw .requiresNewerSystem(required: minimum, current: systemVersion.description)
            }
        }
        try codeSignature.checkInstalledApp(at: app.url)
        let requirement = try codeSignature.designatedRequirement(ofInstalledApp: app.url)
        try codeSignature.checkCandidate(at: candidate, designatedRequirement: requirement, trustedRequirements: trustedRequirements)
        return requirement
    }

    /// Après l'échange : le bundle installé est bien la version préparée, de la même app, signée
    /// comme l'exigeait l'app remplacée.
    private func checkInstalledBundle(expectedVersion: SemanticVersion, designatedRequirement: String) throws(InstallationError) {
        let info = try BundleInfo(at: app.url)
        guard info.identifier == app.bundleIdentifier else {
            throw .bundleIdentifierMismatch(expected: app.bundleIdentifier, found: info.identifier)
        }
        guard (try? SemanticVersion.parseClient(info.shortVersion)) == expectedVersion else {
            throw .bundleVersionMismatch(found: info.shortVersion, expected: expectedVersion.description)
        }
        try codeSignature.checkCandidate(at: app.url, designatedRequirement: designatedRequirement, trustedRequirements: trustedRequirements)
    }

    /// Défait l'échange d'un bundle refusé à sa place définitive : l'ancienne version, mise de côté
    /// à `previous`, reprend sa place, et le bundle refusé retourne dans le dossier de travail. Si
    /// elle ne la reprend pas, elle reste dans ce dossier : dans le dossier de sauvegarde, ou à
    /// défaut à `previous`. L'erreur dit où elle est, et `discard` ne supprime pas le dossier de
    /// travail tant qu'il la contient.
    private func restore(_ previous: URL, of prepared: PreparedUpdate, refusal: InstallationError) throws(InstallationError) {
        let directory = backupDirectory(of: prepared)
        do {
            try BundleFiles.restore(previous: previous, installed: app.url, candidate: prepared.bundleURL, backupDirectory: directory)
        } catch {
            let backup = directory.appending(path: app.url.lastPathComponent, directoryHint: .isDirectory)
            let kept = (BundleFiles.isEmptyOrMissing(directory) ? previous : backup).path(percentEncoded: false)
            log.error("ancienne version gardée dans \(kept, privacy: .public) : \(error.detail, privacy: .public)")
            throw .exchange("\(refusal.detail) ; l'ancienne version n'a pas repris sa place (\(error.detail)), elle est restée dans \(kept)")
        }
    }

    /// Où l'ancienne version est mise de côté, quand l'échange ou son retour n'est pas atomique.
    private func backupDirectory(of prepared: PreparedUpdate) -> URL {
        prepared.workDirectory.appending(path: "previous", directoryHint: .isDirectory)
    }

    /// Dossier de travail sur le volume de l'app, pour que l'échange soit un simple renommage.
    private func workDirectory() throws(InstallationError) -> URL {
        do {
            return try FileManager.default.url(
                for: .itemReplacementDirectory, in: .userDomainMask, appropriateFor: app.url, create: true
            )
        } catch {
            throw .fileSystem("dossier de travail impossible : \(error.localizedDescription)")
        }
    }
}
