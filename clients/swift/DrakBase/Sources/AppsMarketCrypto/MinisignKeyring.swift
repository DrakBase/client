import Foundation

/// Clés publiques qu'une app embarque (`DrakBasePublicKeys`) : la principale, puis celles de
/// secours, gardées hors ligne. Une signature se vérifie avec la clé dont l'identifiant est le sien
/// (`docs/PROTOCOLE.md`, « Clés »).
public struct MinisignKeyring: Hashable, Sendable {
    /// Clé qui signe d'ordinaire les versions de l'app.
    public let primary: MinisignPublicKey
    public let backups: [MinisignPublicKey]

    public init(primary: MinisignPublicKey, backups: [MinisignPublicKey] = []) {
        self.primary = primary
        self.backups = backups
    }

    /// Toutes les clés, la principale en premier.
    public var keys: [MinisignPublicKey] {
        [primary] + backups
    }

    /// Clé qui a fait la signature. Le format historique est refusé en premier, comme dans l'ordre
    /// du protocole ; si aucune clé ne correspond, c'est `keyIDMismatch`, avec la clé principale
    /// comme clé attendue.
    public func key(for signature: MinisignSignature) throws(SignatureError) -> MinisignPublicKey {
        guard signature.isPrehashed else { throw .legacy }
        guard let key = keys.first(where: { $0.keyID == signature.keyID }) else {
            throw .keyIDMismatch(expected: primary.keyID, found: signature.keyID)
        }
        return key
    }

    /// Vérifie une signature sur des données en mémoire, avec la clé qui l'a faite.
    public func verify(_ data: some DataProtocol, signature: MinisignSignature) throws(SignatureError) -> VerifiedSignature {
        try key(for: signature).verify(data, signature: signature)
    }

    /// Vérifie un fichier avec la clé qui l'a signé, hors de l'acteur de l'appelant
    /// (voir `MinisignPublicKey.verifyFile`).
    ///
    /// - Throws: `SignatureError`, `CancellationError`, ou l'erreur de lecture du fichier.
    @concurrent
    public func verifyFile(
        at url: URL, signature: MinisignSignature, chunkByteCount: Int = 1 << 20
    ) async throws -> VerifiedSignature {
        try await key(for: signature).verifyFile(at: url, signature: signature, chunkByteCount: chunkByteCount)
    }
}
