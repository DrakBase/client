import Foundation

/// Refus d'une clé ou d'une signature. Chaque code de `fixtures/README.md` a son cas : `legacy`,
/// `keyIDMismatch` (`key_id_mismatch`), `invalid`, `missingVersion` et `versionMismatch`. Mêmes
/// erreurs que `am_protocol::signature::SignatureError`.
public enum SignatureError: Error, Equatable, Sendable, LocalizedError {
    /// L'enveloppe Tauri n'est pas du base64 standard.
    case base64
    /// Le texte minisign décodé n'est pas de l'UTF-8.
    case utf8
    case malformedPublicKey
    case malformedSignature
    /// Format historique « Ed », non préhaché : toujours refusé.
    case legacy
    /// La signature vient d'une autre clé que celle de l'app.
    case keyIDMismatch(expected: MinisignKeyID, found: MinisignKeyID)
    /// La signature du fichier ou la signature globale ne correspond pas.
    case invalid
    /// Le commentaire de confiance, authentifié, ne contient pas de champ `version:`.
    case missingVersion
    /// La version signée n'est pas celle qu'annonce le serveur.
    case versionMismatch(signed: String, expected: String)

    public var errorDescription: String? {
        switch self {
        case .base64:
            "encodage base64 invalide"
        case .utf8:
            "texte minisign non UTF-8"
        case .malformedPublicKey:
            "clé publique minisign illisible"
        case .malformedSignature:
            "signature minisign illisible"
        case .legacy:
            "signature au format historique non préhaché : refusée"
        case .keyIDMismatch(let expected, let found):
            "signature faite avec la clé \(found), attendue \(expected)"
        case .invalid:
            "signature invalide"
        case .missingVersion:
            "le commentaire de confiance ne contient pas de version"
        case .versionMismatch(let signed, let expected):
            "version signée « \(signed) », attendue « \(expected) »"
        }
    }
}
