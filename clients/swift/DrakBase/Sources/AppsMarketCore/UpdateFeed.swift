import Foundation

/// Adresse de vérification des mises à jour d'une app, lue dans `DrakBaseFeedURL` de son
/// `Info.plist` signé : par exemple `https://maj.example.fr/api/v1/update/translate`.
public struct UpdateFeed: Hashable, Sendable {
    /// Clé de l'`Info.plist` qui contient l'adresse.
    public static let infoPlistKey = "DrakBaseFeedURL"
    /// Son ancien nom, encore lu quand `DrakBaseFeedURL` manque : les apps configurées avant le
    /// renommage continuent de fonctionner.
    package static let legacyInfoPlistKey = "AppsMarketFeedURL"

    public let baseURL: URL

    /// Exige une adresse `https` (ou `http`, pour un serveur local) absolue, sans requête ni
    /// fragment : le paquet y ajoute lui-même la cible, l'arch, la version et les paramètres.
    public init(baseURL: URL) throws(UpdateFeedError) {
        guard let components = URLComponents(url: baseURL, resolvingAgainstBaseURL: true),
              let scheme = components.scheme?.lowercased(), scheme == "https" || scheme == "http",
              let host = components.host, !host.isEmpty,
              components.query == nil, components.fragment == nil
        else {
            throw .invalidFeedURL(baseURL.absoluteString)
        }
        self.baseURL = baseURL
    }

    /// Lit `DrakBaseFeedURL`, sinon l'ancienne `AppsMarketFeedURL`, dans le contenu d'un `Info.plist`.
    public init(infoDictionary: [String: Any]) throws(UpdateFeedError) {
        guard let value = infoDictionary[Self.infoPlistKey] ?? infoDictionary[Self.legacyInfoPlistKey] else {
            throw .missingFeedURL
        }
        guard let string = value as? String, let url = URL(string: string) else {
            throw .invalidFeedURL(String(describing: value))
        }
        try self.init(baseURL: url)
    }

    /// Adresse de la vérification : `{base}/darwin/{arch}/{version}?bundle_type=app&channel=…&os_version=…`.
    /// L'identifiant d'installation n'y figure jamais : il part en en-tête.
    ///
    /// - Parameters:
    ///   - currentVersion: version installée, envoyée sous sa forme canonique.
    ///   - architecture: arch du processus, celle de la tranche du binaire qui tourne.
    ///   - osVersion: version de macOS, pour écarter les versions qui en exigent une plus récente.
    public func checkURL(
        currentVersion: SemanticVersion,
        channel: UpdateChannel,
        architecture: CPUArchitecture = .current,
        osVersion: OperatingSystemVersion = ProcessInfo.processInfo.operatingSystemVersion
    ) -> URL {
        // La forme canonique d'une version ne contient que des caractères permis dans un chemin.
        baseURL
            .appending(path: "darwin/\(architecture.rawValue)/\(currentVersion)", directoryHint: .notDirectory)
            .appending(queryItems: [
                URLQueryItem(name: "bundle_type", value: "app"),
                URLQueryItem(name: "channel", value: channel.rawValue),
                URLQueryItem(name: "os_version", value: Self.osVersionString(osVersion)),
            ])
    }

    /// `14.6` ou `14.6.1`, comme `sw_vers -productVersion`.
    static func osVersionString(_ version: OperatingSystemVersion) -> String {
        let base = "\(version.majorVersion).\(version.minorVersion)"
        return version.patchVersion == 0 ? base : "\(base).\(version.patchVersion)"
    }
}

/// Architecture demandée au serveur, sous les noms de Tauri.
public enum CPUArchitecture: String, Hashable, Sendable {
    case aarch64
    case x86_64

    /// Arch du processus : sous Rosetta, une app universelle tourne en `x86_64` et demande donc
    /// cette arch.
    public static var current: CPUArchitecture {
        #if arch(arm64)
        return .aarch64
        #elseif arch(x86_64)
        return .x86_64
        #else
        #error("DrakBase ne connaît que les Mac Intel et Apple silicon")
        #endif
    }
}

public enum UpdateFeedError: Error, Equatable, Sendable, LocalizedError {
    /// L'`Info.plist` ne contient pas `DrakBaseFeedURL`.
    case missingFeedURL
    /// `DrakBaseFeedURL` n'est pas une adresse http(s) absolue, sans requête ni fragment.
    case invalidFeedURL(String)

    public var errorDescription: String? {
        switch self {
        case .missingFeedURL:
            "DrakBaseFeedURL absent de l'Info.plist"
        case .invalidFeedURL(let value):
            "DrakBaseFeedURL « \(value) » invalide : adresse http(s) absolue attendue, sans requête ni fragment"
        }
    }
}
