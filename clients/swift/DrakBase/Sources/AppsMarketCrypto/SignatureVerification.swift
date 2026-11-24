import Foundation

// Vérification, dans l'ordre obligatoire de `docs/PROTOCOLE.md` : format « ED », identifiant de
// clé, signature Ed25519 du BLAKE2b-512 du fichier, signature globale, et seulement ensuite la
// lecture du commentaire de confiance, qui n'est authentifié que par la signature globale.

extension MinisignPublicKey {
    /// Commence une vérification en flux. Le format historique et une autre clé sont refusés
    /// avant de lire la moindre donnée.
    public func verifier(for signature: MinisignSignature) throws(SignatureError) -> SignatureVerifier {
        guard signature.isPrehashed else { throw .legacy }
        guard signature.keyID == keyID else {
            throw .keyIDMismatch(expected: keyID, found: signature.keyID)
        }
        return SignatureVerifier(publicKey: self, signature: signature)
    }

    /// Vérifie une signature sur des données en mémoire.
    public func verify(_ data: some DataProtocol, signature: MinisignSignature) throws(SignatureError) -> VerifiedSignature {
        var verifier = try verifier(for: signature)
        verifier.update(data: data)
        return try verifier.finalize()
    }

    /// Vérifie un fichier, lu par morceaux de `chunkByteCount` octets. Le calcul tourne hors de
    /// l'acteur de l'appelant, même appelé depuis le MainActor, et s'arrête si la tâche est annulée.
    ///
    /// - Throws: `SignatureError`, `CancellationError`, ou l'erreur de lecture du fichier.
    @concurrent
    public func verifyFile(
        at url: URL, signature: MinisignSignature, chunkByteCount: Int = 1 << 20
    ) async throws -> VerifiedSignature {
        precondition(chunkByteCount > 0, "taille de morceau nulle")
        var verifier = try verifier(for: signature)
        let file = try FileHandle(forReadingFrom: url)
        defer { try? file.close() }
        while let chunk = try file.read(upToCount: chunkByteCount), !chunk.isEmpty {
            try Task.checkCancellation()
            verifier.update(data: chunk)
        }
        return try verifier.finalize()
    }
}

/// Vérification en cours, alimentée morceau par morceau. Obtenue par
/// `MinisignPublicKey.verifier(for:)`, qui a déjà contrôlé le format et la clé.
public struct SignatureVerifier: Sendable {
    private let publicKey: MinisignPublicKey
    private let signature: MinisignSignature
    private var hasher = BLAKE2b512()

    init(publicKey: MinisignPublicKey, signature: MinisignSignature) {
        self.publicKey = publicKey
        self.signature = signature
    }

    public mutating func update(data: some DataProtocol) {
        hasher.update(data: data)
    }

    /// Contrôle la signature du fichier, puis la signature globale. Le commentaire de confiance
    /// n'est rendu qu'une fois authentifié.
    public func finalize() throws(SignatureError) -> VerifiedSignature {
        guard publicKey.isValidSignature(signature.fileSignature, for: hasher.finalize()) else {
            throw .invalid
        }
        let global = signature.fileSignature + Array(signature.trustedComment.utf8)
        guard publicKey.isValidSignature(signature.globalSignature, for: global) else {
            throw .invalid
        }
        return VerifiedSignature(keyID: signature.keyID, trustedComment: signature.trustedComment)
    }
}

/// Résultat d'une vérification réussie : la clé qui a signé et le commentaire de confiance,
/// désormais authentifié.
public struct VerifiedSignature: Hashable, Sendable {
    public let keyID: MinisignKeyID
    /// Champs `clé:valeur` séparés par des tabulations :
    /// `timestamp:<secondes Unix>\tfile:<nom du fichier>\tversion:<version canonique>`.
    public let trustedComment: String

    init(keyID: MinisignKeyID, trustedComment: String) {
        self.keyID = keyID
        self.trustedComment = trustedComment
    }

    /// Valeur du premier champ `name` du commentaire de confiance. Le nom s'arrête au premier `:`,
    /// et les noms se comparent octet par octet.
    public func field(_ name: String) -> String? {
        for field in trustedComment.utf8.split(separator: UInt8(ascii: "\t"), omittingEmptySubsequences: false) {
            guard let colon = field.firstIndex(of: UInt8(ascii: ":")) else { continue }
            if field[..<colon].elementsEqual(name.utf8) {
                return String(decoding: field[field.index(after: colon)...], as: UTF8.self)
            }
        }
        return nil
    }

    /// Exige que la version signée soit exactement `expected`, la version canonique annoncée par
    /// le serveur. `AppsMarketCore` ajoute la même fonction pour une `SemanticVersion`.
    public func requireVersion(_ expected: String) throws(SignatureError) {
        guard let signed = field("version") else { throw .missingVersion }
        guard signed.utf8.elementsEqual(expected.utf8) else {
            throw .versionMismatch(signed: signed, expected: expected)
        }
    }
}
