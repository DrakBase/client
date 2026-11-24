import Foundation

/// Fausses apps dans des dossiers temporaires, pour essayer l'installation sans toucher aux vraies.
public enum FakeApps {
    /// Dossier temporaire neuf, liens résolus (`/private/var/…`), à supprimer par l'appelant.
    public static func temporaryDirectory() throws -> URL {
        let directory = FileManager.default.temporaryDirectory
            .appending(path: "DrakBaseTests-\(UUID().uuidString)", directoryHint: .isDirectory)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        return directory.resolvingSymlinksInPath()
    }

    /// Bundle minimal : `Info.plist` et un exécutable, un script qui ne fait rien. Pas de signature
    /// de code : les tests qui en ont besoin la remplacent, ou signent ad hoc.
    @discardableResult
    public static func make(
        in directory: URL,
        name: String = "Essai",
        bundleIdentifier: String = "fr.example.essai",
        version: String,
        minimumSystemVersion: String? = "14.0",
        info extra: [String: Any] = [:]
    ) throws -> URL {
        let app = directory.appending(path: "\(name).app", directoryHint: .isDirectory)
        let macOS = app.appending(path: "Contents/MacOS", directoryHint: .isDirectory)
        try FileManager.default.createDirectory(at: macOS, withIntermediateDirectories: true)
        var info: [String: Any] = [
            "CFBundleExecutable": name,
            "CFBundleIdentifier": bundleIdentifier,
            "CFBundleName": name,
            "CFBundlePackageType": "APPL",
            "CFBundleShortVersionString": version,
            "CFBundleVersion": "1",
        ]
        if let minimumSystemVersion {
            info["LSMinimumSystemVersion"] = minimumSystemVersion
        }
        info.merge(extra) { _, new in new }
        try writeInfo(info, to: app)
        let executable = macOS.appending(path: name, directoryHint: .notDirectory)
        try Data("#!/bin/sh\nexit 0\n".utf8).write(to: executable)
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: executable.path(percentEncoded: false))
        return app
    }

    public static func writeInfo(_ info: [String: Any], to app: URL) throws {
        let data = try PropertyListSerialization.data(fromPropertyList: info, format: .xml, options: 0)
        try data.write(to: app.appending(path: "Contents/Info.plist", directoryHint: .notDirectory))
    }

    public static func info(of app: URL) throws -> [String: Any] {
        let data = try Data(contentsOf: app.appending(path: "Contents/Info.plist", directoryHint: .notDirectory))
        return try PropertyListSerialization.propertyList(from: data, format: nil) as? [String: Any] ?? [:]
    }

    public static func version(of app: URL) throws -> String? {
        try info(of: app)["CFBundleShortVersionString"] as? String
    }

    /// Archive `.tar.gz` comme `pack-macos` : les éléments de `items`, tous dans le même dossier,
    /// à la racine de l'archive, sans fichiers AppleDouble `._*`.
    public static func archive(_ items: [URL], to destination: URL) throws {
        guard let parent = items.first?.deletingLastPathComponent() else { return }
        try run(
            "/usr/bin/tar",
            ["-czf", destination.path(percentEncoded: false), "-C", parent.path(percentEncoded: false)] + items.map(\.lastPathComponent),
            environment: ["COPYFILE_DISABLE": "1"]
        )
    }

    /// Signe ad hoc, comme Xcode pour « Sign to Run Locally ». Aucun certificat ni trousseau.
    public static func signAdHoc(_ app: URL) throws {
        try run("/usr/bin/codesign", ["--force", "--sign", "-", app.path(percentEncoded: false)])
    }

    /// Pose l'attribut de quarantaine d'un fichier téléchargé par un navigateur.
    public static func quarantine(_ url: URL) throws {
        let value = "0083;\(String(Int(Date().timeIntervalSince1970), radix: 16));Safari;"
        try run("/usr/bin/xattr", ["-w", "com.apple.quarantine", value, url.path(percentEncoded: false)])
    }

    public static func hasQuarantine(_ url: URL) -> Bool {
        url.withUnsafeFileSystemRepresentation { path in
            getxattr(path, "com.apple.quarantine", nil, 0, 0, XATTR_NOFOLLOW) >= 0
        }
    }

    /// Lance un outil et attend sa fin ; échoue avec sa sortie d'erreur.
    @discardableResult
    public static func run(_ tool: String, _ arguments: [String], environment: [String: String] = [:]) throws -> String {
        let process = Process()
        process.executableURL = URL(filePath: tool)
        process.arguments = arguments
        process.environment = ProcessInfo.processInfo.environment.merging(environment) { _, new in new }
        let output = Pipe()
        process.standardOutput = output
        process.standardError = output
        try process.run()
        let data = output.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        let text = String(decoding: data, as: UTF8.self)
        guard process.terminationStatus == 0 else {
            throw ToolError(description: "\(tool) \(arguments.joined(separator: " ")) : \(text)")
        }
        return text
    }

    public struct ToolError: Error, CustomStringConvertible {
        public let description: String
    }
}
