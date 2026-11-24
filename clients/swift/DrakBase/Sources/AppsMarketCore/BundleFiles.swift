import Darwin
import Foundation

/// Ce que l'installation lit dans l'`Info.plist` d'un bundle sur le disque. La lecture est directe :
/// `Bundle(url:)` garde un cache par chemin, faux après un échange.
struct BundleInfo: Equatable, Sendable {
    let identifier: String
    let shortVersion: String
    let minimumSystemVersion: String?

    init(at bundle: URL) throws(InstallationError) {
        let plist = bundle.appending(path: "Contents/Info.plist", directoryHint: .notDirectory)
        let object: Any
        do {
            object = try PropertyListSerialization.propertyList(from: Data(contentsOf: plist), format: nil)
        } catch {
            throw .unreadableBundle("Info.plist illisible dans \(bundle.lastPathComponent)")
        }
        guard let info = object as? [String: Any] else {
            throw .unreadableBundle("Info.plist n'est pas un dictionnaire")
        }
        guard let identifier = info["CFBundleIdentifier"] as? String, !identifier.isEmpty else {
            throw .unreadableBundle("CFBundleIdentifier absent")
        }
        guard let shortVersion = info["CFBundleShortVersionString"] as? String else {
            throw .unreadableBundle("CFBundleShortVersionString absent")
        }
        let minimum = info["LSMinimumSystemVersion"]
        guard minimum == nil || minimum is String else {
            throw .unreadableBundle("LSMinimumSystemVersion n'est pas une chaîne")
        }
        self.identifier = identifier
        self.shortVersion = shortVersion
        minimumSystemVersion = minimum as? String
    }
}

/// Opérations sur les fichiers de l'installation, par le système seulement.
enum BundleFiles {
    static let quarantineAttribute = "com.apple.quarantine"

    /// Extrait une archive `.tar.gz` avec `/usr/bin/tar`, qui garde les liens symboliques et refuse
    /// les chemins absolus ou qui remontent au-dessus du dossier.
    static func extract(_ archive: URL, into directory: URL) throws(InstallationError) {
        let process = Process()
        process.executableURL = URL(filePath: "/usr/bin/tar")
        process.arguments = ["-xzf", archive.path(percentEncoded: false), "-C", directory.path(percentEncoded: false)]
        process.standardInput = FileHandle.nullDevice
        process.standardOutput = FileHandle.nullDevice
        let errors = Pipe()
        process.standardError = errors
        do {
            try process.run()
        } catch {
            throw .extraction("lancement de /usr/bin/tar impossible : \(error.localizedDescription)")
        }
        // Lu jusqu'au bout avant d'attendre la fin : un tube plein bloquerait tar.
        let output = errors.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        guard process.terminationReason == .exit, process.terminationStatus == 0 else {
            let message = String(decoding: output, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
            throw .extraction(message.isEmpty ? "tar a échoué (\(process.terminationStatus))" : message)
        }
    }

    /// Le seul élément de `directory`, qui doit être un dossier `.app`, pas un lien.
    static func singleApp(in directory: URL) throws(InstallationError) -> URL {
        let entries: [URL]
        do {
            entries = try FileManager.default.contentsOfDirectory(
                at: directory, includingPropertiesForKeys: [.isDirectoryKey, .isSymbolicLinkKey], options: []
            )
        } catch {
            throw .fileSystem(error.localizedDescription)
        }
        guard entries.count == 1, let entry = entries.first else {
            let names = entries.map(\.lastPathComponent).sorted().joined(separator: ", ")
            throw .archiveLayout(entries.isEmpty ? "archive vide" : "trouvé \(names)")
        }
        let values = try? entry.resourceValues(forKeys: [.isDirectoryKey, .isSymbolicLinkKey])
        guard entry.pathExtension == "app", values?.isDirectory == true, values?.isSymbolicLink != true else {
            throw .archiveLayout("trouvé \(entry.lastPathComponent)")
        }
        return entry
    }

    /// Retire l'attribut de quarantaine du bundle et de tout ce qu'il contient, liens compris, sans
    /// les suivre : `tar` le recopie de l'archive sur chaque fichier extrait.
    static func removeQuarantine(from root: URL) throws(InstallationError) {
        var items = [root]
        if let enumerator = FileManager.default.enumerator(at: root, includingPropertiesForKeys: nil, options: []) {
            for case let url as URL in enumerator {
                items.append(url)
            }
        }
        for item in items {
            let failure = item.withUnsafeFileSystemRepresentation { path -> Int32? in
                removexattr(path, quarantineAttribute, XATTR_NOFOLLOW) == 0 || errno == ENOATTR ? nil : errno
            }
            if let failure {
                throw .fileSystem("retrait de la quarantaine de \(item.lastPathComponent) impossible : \(message(failure))")
            }
        }
    }

    /// Échange atomique du bundle préparé et de l'app installée (`renamex_np` avec `RENAME_SWAP`).
    /// Si le volume ne le permet pas, repli sur deux renommages, l'ancienne version étant mise de
    /// côté dans `backupDirectory`. Rend l'emplacement de l'ancienne version.
    static func exchange(candidate: URL, installed: URL, backupDirectory: URL) throws(InstallationError) -> URL {
        let swapFailure = posix(candidate, installed) { renamex_np($0, $1, UInt32(RENAME_SWAP)) }
        guard let swapFailure else { return candidate }
        do {
            return try exchangeWithBackup(candidate: candidate, installed: installed, backupDirectory: backupDirectory)
        } catch {
            throw .exchange("échange atomique refusé (\(message(swapFailure))), puis \(error.detail)")
        }
    }

    /// Repli de l'échange : l'app installée part dans `backupDirectory`, puis le bundle préparé
    /// prend sa place. Si ce second renommage échoue, l'app installée est remise en place.
    static func exchangeWithBackup(candidate: URL, installed: URL, backupDirectory: URL) throws(InstallationError) -> URL {
        let backup = backupDirectory.appending(path: installed.lastPathComponent, directoryHint: .isDirectory)
        do {
            try FileManager.default.createDirectory(at: backupDirectory, withIntermediateDirectories: true)
        } catch {
            throw .exchange("dossier de sauvegarde impossible : \(error.localizedDescription)")
        }
        if let failure = posix(installed, backup, { rename($0, $1) }) {
            throw .exchange("mise de côté de l'app installée impossible : \(message(failure))")
        }
        if let failure = posix(candidate, installed, { rename($0, $1) }) {
            let restored = posix(backup, installed) { rename($0, $1) } == nil
            let location = restored ? "" : " ; l'app installée est restée dans \(backup.path(percentEncoded: false))"
            throw .exchange("mise en place de la nouvelle version impossible : \(message(failure))\(location)")
        }
        return backup
    }

    /// Défait un échange : l'ancienne version, mise de côté à `previous`, reprend la place
    /// d'`installed`, dont le bundle retourne à `candidate`, d'où il venait.
    ///
    /// Après un échange atomique, `previous` est à `candidate` : un second échange suffit. Sinon, ou
    /// si le volume le refuse, des renommages passent par `backupDirectory`, comme
    /// `exchangeWithBackup`. Quand la remise en place échoue, l'ancienne version est dans
    /// `backupDirectory`, ou à défaut restée à `previous`.
    static func restore(previous: URL, installed: URL, candidate: URL, backupDirectory: URL) throws(InstallationError) {
        let swapped = previous == candidate
        if swapped, posix(previous, installed, { renamex_np($0, $1, UInt32(RENAME_SWAP)) }) == nil {
            return
        }
        let backup = backupDirectory.appending(path: installed.lastPathComponent, directoryHint: .isDirectory)
        if swapped {
            do {
                try FileManager.default.createDirectory(at: backupDirectory, withIntermediateDirectories: true)
            } catch {
                throw .exchange("dossier de sauvegarde impossible : \(error.localizedDescription)")
            }
            if let failure = posix(previous, backup, { rename($0, $1) }) {
                throw .exchange("mise de côté de l'ancienne version impossible : \(message(failure))")
            }
        }
        if let failure = posix(installed, candidate, { rename($0, $1) }) {
            throw .exchange("retrait du bundle refusé impossible : \(message(failure))")
        }
        if let failure = posix(backup, installed, { rename($0, $1) }) {
            throw .exchange("remise en place de l'ancienne version impossible : \(message(failure))")
        }
    }

    /// Vrai si `directory` n'existe pas, ou ne contient rien.
    static func isEmptyOrMissing(_ directory: URL) -> Bool {
        let entries = try? FileManager.default.contentsOfDirectory(atPath: directory.path(percentEncoded: false))
        return entries?.isEmpty ?? true
    }

    /// Vrai si le processus peut modifier `url` (`access(2)` avec `W_OK`) : faux sur un volume en
    /// lecture seule, comme une image disque.
    static func isWritable(_ url: URL) -> Bool {
        url.withUnsafeFileSystemRepresentation { path in
            access(path, W_OK) == 0
        }
    }

    /// Vrai si `url` est `directory` ou se trouve dedans, liens symboliques résolus.
    static func url(_ url: URL, isInside directory: URL) -> Bool {
        let item = url.resolvingSymlinksInPath().standardizedFileURL.pathComponents
        let parent = directory.resolvingSymlinksInPath().standardizedFileURL.pathComponents
        return item.count >= parent.count && Array(item.prefix(parent.count)) == parent
    }

    /// Appel POSIX sur deux chemins : `nil` s'il réussit, sinon `errno`, lu aussitôt.
    private static func posix(_ first: URL, _ second: URL, _ call: (UnsafePointer<CChar>, UnsafePointer<CChar>) -> Int32) -> Int32? {
        first.withUnsafeFileSystemRepresentation { firstPath in
            second.withUnsafeFileSystemRepresentation { secondPath in
                guard let firstPath, let secondPath else { return EINVAL }
                return call(firstPath, secondPath) == 0 ? nil : errno
            }
        }
    }

    static func message(_ code: Int32) -> String {
        String(cString: strerror(code))
    }
}
