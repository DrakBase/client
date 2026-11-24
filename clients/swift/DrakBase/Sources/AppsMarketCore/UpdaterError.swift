import AppsMarketCrypto
import Foundation

/// Échec d'une vérification, d'un téléchargement, d'une installation ou de la relance.
public enum UpdaterError: Error, Equatable, Sendable, LocalizedError {
    /// Le serveur n'a pas pu être joint.
    case network(String)
    /// Statut HTTP inattendu, avec le message d'erreur du serveur s'il en donne un.
    case server(status: Int, message: String?)
    /// Réponse illisible ou hors contrat.
    case invalidResponse(String)
    /// Le serveur a envoyé une autre taille que celle annoncée.
    case sizeMismatch(expected: UInt64, received: UInt64)
    case checksumMismatch
    /// Écriture de l'archive dans Caches impossible.
    case storage(String)
    /// L'archive téléchargée ne passe pas la vérification minisign.
    case signature(SignatureError)
    case installation(InstallationError)
    /// La mise à jour est installée, mais l'app n'a pas pu se relancer.
    case relaunch(String)
    /// L'installation faite à la fermeture précédente a échoué ; montrée au lancement suivant.
    case installationAtQuit(String)

    public var errorDescription: String? {
        switch self {
        case .network(let detail):
            "serveur de mises à jour injoignable : \(detail)"
        case .server(410, _):
            "cette version a été retirée par son éditeur"
        case .server(let status, let message):
            "le serveur de mises à jour a répondu \(status)" + (message.map { " : \($0)" } ?? "")
        case .invalidResponse(let detail):
            "réponse du serveur illisible : \(detail)"
        case .sizeMismatch(let expected, let received):
            "téléchargement incomplet ou trop long : \(received) octets reçus, \(expected) annoncés"
        case .checksumMismatch:
            "archive téléchargée corrompue : son empreinte SHA-256 n'est pas celle annoncée"
        case .storage(let detail):
            "enregistrement de l'archive impossible : \(detail)"
        case .signature(let error):
            "signature de la mise à jour refusée : \(error.localizedDescription)"
        case .installation(let error):
            error.localizedDescription
        case .relaunch(let detail):
            "la mise à jour est installée, mais l'app n'a pas pu se relancer (\(detail)) : quittez-la et rouvrez-la"
        case .installationAtQuit(let detail):
            "l'installation à la fermeture de l'app a échoué : \(detail)"
        }
    }
}
