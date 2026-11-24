/// Signature d'un fichier, lue depuis un `.sig` au format Tauri, ou depuis le champ `signature`
/// de la réponse du serveur. Rien n'y est encore vérifié : le commentaire de confiance n'est
/// accessible qu'après vérification, dans `VerifiedSignature`.
public struct MinisignSignature: Hashable, Sendable {
    public let keyID: MinisignKeyID
    /// Algorithme « ED » : Ed25519 sur le BLAKE2b-512 du fichier. Faux pour le format historique
    /// « Ed », lu pour pouvoir le refuser explicitement.
    public let isPrehashed: Bool
    /// Signature Ed25519 du fichier, ou de son empreinte (64 octets).
    let fileSignature: [UInt8]
    /// Texte qui suit `trusted comment: `, authentifié par la signature globale seulement.
    let trustedComment: String
    /// Signature Ed25519 de `fileSignature ‖ trustedComment` (64 octets).
    let globalSignature: [UInt8]

    private static let trustedCommentPrefix = Array("trusted comment: ".utf8)

    /// Lit une signature au format Tauri : base64 standard du texte complet du `.sig`.
    public init(tauri encoded: String) throws(SignatureError) {
        try self.init(minisignText: MinisignText.decodeTauri(encoded))
    }

    /// Lit le texte minisign d'une signature, en quatre lignes : commentaire non signé
    /// (`untrusted comment:`), signature, commentaire de confiance, signature globale. Une app ne
    /// reçoit que le format Tauri : cette lecture sert aux jeux d'essai du paquet.
    package init(minisign text: String) throws(SignatureError) {
        try self.init(minisignText: Array(text.utf8))
    }

    private init(minisignText text: [UInt8]) throws(SignatureError) {
        let lines = MinisignText.lines(text)
        guard lines.count >= 4, lines[0].starts(with: MinisignText.untrustedCommentPrefix),
              let data = StrictBase64.decode(Array(lines[1])),
              data.count == 2 + 8 + 64,
              lines[2].starts(with: Self.trustedCommentPrefix),
              let globalSignature = StrictBase64.decode(Array(lines[3])),
              globalSignature.count == 64
        else {
            throw .malformedSignature
        }
        switch (data[0], data[1]) {
        case (UInt8(ascii: "E"), UInt8(ascii: "D")):
            isPrehashed = true
        case (UInt8(ascii: "E"), UInt8(ascii: "d")):
            isPrehashed = false
        default:
            throw .malformedSignature
        }
        keyID = MinisignKeyID(data[2..<10])
        fileSignature = Array(data[10...])
        // Le texte entier est de l'UTF-8 valide et le préfixe est en ASCII : la suite l'est aussi.
        trustedComment = String(decoding: lines[2].dropFirst(Self.trustedCommentPrefix.count), as: UTF8.self)
        self.globalSignature = globalSignature
    }
}
