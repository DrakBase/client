import AppsMarketCrypto
import Foundation

/// Canal de mises à jour suivi par l'app. La bêta voit aussi les versions stables, donc toujours
/// la plus haute version publiée.
public enum UpdateChannel: String, Hashable, Sendable, Codable {
    case stable
    case beta
}

/// Corps de la réponse 200 du point de vérification (`docs/PROTOCOLE.md`). Les noms et les règles
/// de lecture sont ceux de `am_protocol::update::UpdateResponse`. L'encodage, avec les mêmes noms,
/// sert à garder une mise à jour préparée d'un lancement à l'autre.
public struct UpdateResponse: Hashable, Sendable, Codable {
    /// Version proposée, toujours canonique côté serveur.
    public let version: SemanticVersion
    /// Adresse de l'archive `.app.tar.gz`.
    public let url: URL
    /// Contenu du `.sig`, tel quel, au format Tauri : à lire avec `MinisignSignature(tauri:)`.
    public let signature: String
    /// Notes de version en Markdown ; absentes si elles sont vides.
    public let notes: String?
    /// Date de publication, en RFC 3339 UTC.
    public let pubDate: String
    /// Taille de l'archive, en octets.
    public let size: UInt64
    /// SHA-256 de l'archive, en hexadécimal.
    public let sha256: String
    /// Version minimale de macOS, absente si elle est inconnue.
    public let minOSVersion: String?
    public let channel: UpdateChannel

    private enum CodingKeys: String, CodingKey {
        case version, url, signature, notes
        case pubDate = "pub_date"
        case size, sha256
        case minOSVersion = "min_os_version"
        case channel
    }
}

extension VerifiedSignature {
    /// Exige que la version signée soit exactement la forme canonique de `expected`, par exemple
    /// la `version` d'une `UpdateResponse`.
    public func requireVersion(_ expected: SemanticVersion) throws(SignatureError) {
        try requireVersion(expected.description)
    }
}
