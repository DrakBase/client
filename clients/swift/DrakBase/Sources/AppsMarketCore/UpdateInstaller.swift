import AppsMarketCrypto
import Foundation

/// Installe une mise à jour déjà téléchargée et vérifiée : préparation du nouveau bundle, puis
/// échange avec l'app installée (`docs/PLAN.md`, « Déroulé », étapes 4 à 6). `LocalInstaller` le
/// fait dans le processus de l'app ; une app sandboxée passera par son service XPC (jalon C2).
///
/// Un installeur ne fait confiance qu'à ce qu'il lit lui-même : il vérifie de nouveau l'archive
/// avec ses propres clés, et contrôle le nouveau bundle avant de le préparer comme avant de
/// l'installer.
public protocol UpdateInstaller: Sendable {
    /// Ce qui empêche l'app de se mettre à jour là où elle est, ou `nil`. Rapide, sans écriture.
    func unavailability() -> UpdateUnavailability?

    /// Copie l'archive dans un dossier de travail sur le volume de l'app, la vérifie de nouveau,
    /// l'extrait et contrôle le nouveau bundle, prêt à être échangé.
    ///
    /// - Throws: `InstallationError`, ou `CancellationError`.
    func prepare(archive: URL, for update: UpdateResponse) async throws -> PreparedUpdate

    /// Contrôle final, puis échange du bundle préparé et de l'app installée. Synchrone, car il sert
    /// aussi pendant la fermeture de l'app ; hors de ce cas, à appeler loin de l'acteur principal.
    ///
    /// - Throws: `InstallationError`.
    func install(_ prepared: PreparedUpdate) throws -> InstalledUpdate

    /// Abandonne une préparation : son dossier de travail est supprimé.
    func discard(_ prepared: PreparedUpdate)
}

/// Nouveau bundle prêt à prendre la place de l'app installée. Gardé dans les préférences, pour que
/// la préparation survive à une relance de l'app.
public struct PreparedUpdate: Hashable, Sendable, Codable {
    /// Version annoncée par le serveur, égale à la version signée et à celle du bundle.
    public let version: SemanticVersion
    public let bundleURL: URL
    /// Dossier de travail, sur le volume de l'app : il contient le bundle, puis l'ancienne version
    /// après l'échange.
    public let workDirectory: URL

    public init(version: SemanticVersion, bundleURL: URL, workDirectory: URL) {
        self.version = version
        self.bundleURL = bundleURL
        self.workDirectory = workDirectory
    }
}

/// Mise à jour installée : la nouvelle version occupe l'emplacement de l'app.
public struct InstalledUpdate: Hashable, Sendable {
    public let version: SemanticVersion
    /// Ancienne version, à supprimer une fois l'ancienne instance fermée (`RelaunchGate`), ou
    /// `nil` si l'installeur s'en est déjà chargé.
    public let previousBundle: URL?

    public init(version: SemanticVersion, previousBundle: URL?) {
        self.version = version
        self.previousBundle = previousBundle
    }
}

/// Raison pour laquelle l'app ne peut pas se mettre à jour, avec le message à montrer.
public enum UpdateUnavailability: Hashable, Sendable {
    /// Configuration absente ou illisible dans l'`Info.plist`.
    case misconfigured(String)
    /// macOS a lancé l'app depuis un emplacement temporaire (translocation de Gatekeeper).
    case translocated
    /// Le dossier de l'app, ou l'app elle-même, n'est pas modifiable.
    case readOnlyLocation(String)
    /// L'app installée est signée ad hoc : son exigence désignée ne vaut que pour son binaire exact.
    case adHocSignature
    case unsigned
    /// L'app passe par un service XPC d'installation, que cette version du paquet ne sait pas
    /// encore utiliser.
    case installerServiceUnsupported(String)

    public var message: String {
        switch self {
        case .misconfigured(let detail):
            "Les mises à jour ne sont pas configurées dans cette app : \(detail)."
        case .translocated:
            "macOS a ouvert l'app depuis un emplacement temporaire. Déplacez-la dans le dossier Applications, puis rouvrez-la."
        case .readOnlyLocation(let path):
            "Le dossier de l'app (\(path)) n'est pas modifiable. Déplacez l'app dans le dossier Applications."
        case .adHocSignature:
            "Cette copie de l'app est signée ad hoc : elle ne peut pas se mettre à jour. Installez-la depuis sa page de téléchargement."
        case .unsigned:
            "Cette copie de l'app n'est pas signée : elle ne peut pas se mettre à jour. Installez-la depuis sa page de téléchargement."
        case .installerServiceUnsupported(let service):
            "Le service d'installation « \(service) » n'est pas encore pris en charge."
        }
    }

    /// Vrai si l'utilisateur peut y remédier en déplaçant l'app : seuls ces cas méritent de
    /// l'avertir sans qu'il ait rien demandé.
    public var isFixableByMovingTheApp: Bool {
        switch self {
        case .translocated, .readOnlyLocation: true
        case .misconfigured, .adHocSignature, .unsigned, .installerServiceUnsupported: false
        }
    }
}

/// Refus ou échec de la préparation ou de l'installation.
public enum InstallationError: Error, Equatable, Sendable, LocalizedError {
    case unavailable(UpdateUnavailability)
    /// La copie de l'archive ne passe pas la vérification minisign.
    case signature(SignatureError)
    case extraction(String)
    /// L'archive ne contient pas exactement un dossier `.app` à sa racine.
    case archiveLayout(String)
    case unreadableBundle(String)
    case bundleIdentifierMismatch(expected: String, found: String)
    /// La version du bundle n'est pas celle annoncée et signée.
    case bundleVersionMismatch(found: String, expected: String)
    /// Le bundle n'est pas plus récent que l'app installée : pas de retour arrière.
    case notNewer(found: String, installed: String)
    case requiresNewerSystem(required: String, current: String)
    case codeSignature(String)
    case fileSystem(String)
    /// L'échange des bundles a échoué ; l'app installée est restée en place.
    case exchange(String)

    public var errorDescription: String? {
        switch self {
        case .unavailable(let reason):
            reason.message
        case .signature(let error):
            "signature de la mise à jour refusée : \(error.localizedDescription)"
        case .extraction(let detail):
            "extraction de l'archive impossible : \(detail)"
        case .archiveLayout(let detail):
            "l'archive doit contenir un seul dossier .app à sa racine : \(detail)"
        case .unreadableBundle(let detail):
            "nouveau bundle illisible : \(detail)"
        case .bundleIdentifierMismatch(let expected, let found):
            "identifiant de bundle « \(found) » au lieu de « \(expected) »"
        case .bundleVersionMismatch(let found, let expected):
            "version du bundle « \(found) » au lieu de « \(expected) », la version annoncée et signée"
        case .notNewer(let found, let installed):
            "la version \(found) n'est pas plus récente que la version installée, \(installed)"
        case .requiresNewerSystem(let required, let current):
            "cette version exige macOS \(required), ce Mac a macOS \(current)"
        case .codeSignature(let detail):
            "signature de code refusée : \(detail)"
        case .fileSystem(let detail):
            "opération sur les fichiers impossible : \(detail)"
        case .exchange(let detail):
            "remplacement de l'app impossible : \(detail)"
        }
    }

    /// Détail d'un refus, sans le préfixe de `errorDescription`, pour l'inclure dans un autre message.
    var detail: String {
        switch self {
        case .extraction(let detail), .archiveLayout(let detail), .unreadableBundle(let detail),
             .codeSignature(let detail), .fileSystem(let detail), .exchange(let detail):
            detail
        default:
            localizedDescription
        }
    }
}

/// Installeur d'une app qui déclare un service XPC d'installation (`DrakBaseInstallerService`),
/// en attendant le jalon C2 : la mise à jour est trouvée, mais reste indisponible.
struct UnsupportedInstaller: UpdateInstaller {
    let service: String

    func unavailability() -> UpdateUnavailability? {
        .installerServiceUnsupported(service)
    }

    func prepare(archive: URL, for update: UpdateResponse) async throws -> PreparedUpdate {
        throw InstallationError.unavailable(.installerServiceUnsupported(service))
    }

    func install(_ prepared: PreparedUpdate) throws -> InstalledUpdate {
        throw InstallationError.unavailable(.installerServiceUnsupported(service))
    }

    func discard(_ prepared: PreparedUpdate) {}
}
