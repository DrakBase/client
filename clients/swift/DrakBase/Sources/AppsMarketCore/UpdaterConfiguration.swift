import AppsMarketCrypto
import Foundation
import Security

/// Réglages de mise à jour d'une app, lus dans son `Info.plist` signé (`docs/PROTOCOLE.md`,
/// « Intégration »). Rien ne vient d'ailleurs : ni du serveur, ni des préférences.
public struct UpdaterConfiguration: Hashable, Sendable {
    /// Tableau des clés publiques au format Tauri : la principale, puis celle de secours. L'adresse
    /// de vérification, elle, est sous `UpdateFeed.infoPlistKey`.
    public static let publicKeysKey = "DrakBasePublicKeys"
    /// Tableau facultatif d'exigences de signature de code, acceptées en plus de l'exigence
    /// désignée de l'app installée : c'est ce qui permet de changer un jour de certificat.
    public static let trustedRequirementsKey = "DrakBaseTrustedRequirements"
    /// Identifiant facultatif du service XPC d'installation, pour une app sandboxée.
    public static let installerServiceKey = "DrakBaseInstallerService"
    /// Ancien nom de chaque clé, `AppsMarket…` au lieu de `DrakBase…`, lu seulement quand la
    /// nouvelle clé manque : une app configurée avant le renommage continue de fonctionner.
    package static func legacyKey(for key: String) -> String {
        key.hasPrefix("DrakBase") ? "AppsMarket" + key.dropFirst("DrakBase".count) : key
    }

    public let feed: UpdateFeed
    public let publicKeys: MinisignKeyring
    /// Exigences de signature de code, au format de `csreq`, déjà contrôlées.
    public let trustedRequirements: [String]
    public let installerService: String?

    /// Contrôle chaque exigence de signature de code : une exigence illisible est une erreur de
    /// l'app, pas une raison de l'ignorer.
    public init(
        feed: UpdateFeed,
        publicKeys: MinisignKeyring,
        trustedRequirements: [String] = [],
        installerService: String? = nil
    ) throws(ConfigurationError) {
        for requirement in trustedRequirements where !Self.isValidRequirement(requirement) {
            throw .invalidTrustedRequirement(requirement)
        }
        if let installerService, installerService.isEmpty {
            throw .invalidInstallerService
        }
        self.feed = feed
        self.publicKeys = publicKeys
        self.trustedRequirements = trustedRequirements
        self.installerService = installerService
    }

    /// Lit la configuration dans l'`Info.plist` d'un bundle, celui de l'app par défaut.
    public init(bundle: Bundle = .main) throws(ConfigurationError) {
        try self.init(infoDictionary: bundle.infoDictionary ?? [:])
    }

    /// Lit la configuration dans le contenu d'un `Info.plist`.
    public init(infoDictionary: [String: Any]) throws(ConfigurationError) {
        let feed: UpdateFeed
        do {
            feed = try UpdateFeed(infoDictionary: infoDictionary)
        } catch {
            throw .feed(error)
        }

        guard let rawKeys = Self.value(of: Self.publicKeysKey, in: infoDictionary) else { throw .missingPublicKeys }
        guard let encodedKeys = rawKeys as? [String], !encodedKeys.isEmpty else { throw .invalidPublicKeys }
        var keys: [MinisignPublicKey] = []
        for (index, encoded) in encodedKeys.enumerated() {
            let key: MinisignPublicKey
            do {
                key = try MinisignPublicKey(tauri: encoded)
            } catch {
                throw .invalidPublicKey(index: index, error)
            }
            // Deux clés de même identifiant rendraient le choix de la clé ambigu.
            guard !keys.contains(where: { $0.keyID == key.keyID }) else { throw .duplicatePublicKey(key.keyID) }
            keys.append(key)
        }

        var requirements: [String] = []
        if let rawRequirements = Self.value(of: Self.trustedRequirementsKey, in: infoDictionary) {
            guard let strings = rawRequirements as? [String] else { throw .invalidTrustedRequirements }
            requirements = strings
        }

        var service: String?
        if let rawService = Self.value(of: Self.installerServiceKey, in: infoDictionary) {
            guard let string = rawService as? String else { throw .invalidInstallerService }
            service = string
        }

        try self.init(
            feed: feed,
            publicKeys: MinisignKeyring(primary: keys[0], backups: Array(keys.dropFirst())),
            trustedRequirements: requirements,
            installerService: service
        )
    }

    /// La valeur d'une clé, sinon celle de son ancien nom.
    private static func value(of key: String, in infoDictionary: [String: Any]) -> Any? {
        infoDictionary[key] ?? infoDictionary[legacyKey(for: key)]
    }

    private static func isValidRequirement(_ text: String) -> Bool {
        var requirement: SecRequirement?
        return SecRequirementCreateWithString(text as CFString, [], &requirement) == errSecSuccess && requirement != nil
    }
}

/// L'app à mettre à jour, telle qu'elle tourne : son emplacement, son identifiant et sa version.
public struct InstalledApp: Hashable, Sendable {
    public let url: URL
    public let bundleIdentifier: String
    /// `CFBundleShortVersionString`, lu comme une version d'app cliente : `1.2` devient `1.2.0`.
    public let version: SemanticVersion

    public init(url: URL, bundleIdentifier: String, version: SemanticVersion) {
        self.url = url
        self.bundleIdentifier = bundleIdentifier
        self.version = version
    }

    /// L'app d'un bundle, celle qui tourne par défaut. Son `Info.plist` est celui qui a été chargé
    /// au lancement, donc celui que couvre la signature.
    public init(bundle: Bundle = .main) throws(ConfigurationError) {
        let info = bundle.infoDictionary ?? [:]
        guard let identifier = info["CFBundleIdentifier"] as? String, !identifier.isEmpty else {
            throw .missingBundleIdentifier
        }
        guard let shortVersion = info["CFBundleShortVersionString"] as? String else {
            throw .missingBundleVersion
        }
        let version: SemanticVersion
        do {
            version = try SemanticVersion.parseClient(shortVersion)
        } catch {
            throw .invalidBundleVersion(shortVersion)
        }
        self.init(url: bundle.bundleURL, bundleIdentifier: identifier, version: version)
    }
}

/// Configuration absente ou illisible : les mises à jour sont alors indisponibles.
public enum ConfigurationError: Error, Equatable, Sendable, LocalizedError {
    case feed(UpdateFeedError)
    case missingPublicKeys
    /// `DrakBasePublicKeys` n'est pas un tableau de chaînes non vide.
    case invalidPublicKeys
    case invalidPublicKey(index: Int, SignatureError)
    case duplicatePublicKey(MinisignKeyID)
    /// `DrakBaseTrustedRequirements` n'est pas un tableau de chaînes.
    case invalidTrustedRequirements
    case invalidTrustedRequirement(String)
    /// `DrakBaseInstallerService` n'est pas une chaîne non vide.
    case invalidInstallerService
    case missingBundleIdentifier
    case missingBundleVersion
    case invalidBundleVersion(String)

    public var errorDescription: String? {
        switch self {
        case .feed(let error):
            error.localizedDescription
        case .missingPublicKeys:
            "DrakBasePublicKeys absent de l'Info.plist"
        case .invalidPublicKeys:
            "DrakBasePublicKeys doit être un tableau non vide de clés au format Tauri"
        case .invalidPublicKey(let index, let error):
            "DrakBasePublicKeys, clé \(index + 1) : \(error.localizedDescription)"
        case .duplicatePublicKey(let keyID):
            "DrakBasePublicKeys contient deux fois la clé \(keyID)"
        case .invalidTrustedRequirements:
            "DrakBaseTrustedRequirements doit être un tableau de chaînes"
        case .invalidTrustedRequirement(let requirement):
            "exigence de signature de code illisible dans DrakBaseTrustedRequirements : « \(requirement) »"
        case .invalidInstallerService:
            "DrakBaseInstallerService doit être un identifiant de service non vide"
        case .missingBundleIdentifier:
            "CFBundleIdentifier absent de l'Info.plist"
        case .missingBundleVersion:
            "CFBundleShortVersionString absent de l'Info.plist"
        case .invalidBundleVersion(let version):
            "CFBundleShortVersionString « \(version) » illisible"
        }
    }
}
