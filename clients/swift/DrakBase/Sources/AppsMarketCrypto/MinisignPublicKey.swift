import CryptoKit

/// Identifiant d'une clé minisign (8 octets), recopié dans chaque signature faite avec elle.
public struct MinisignKeyID: Hashable, Sendable, CustomStringConvertible {
    /// Les 8 octets lus comme un entier petit-boutiste, comme minisign les affiche.
    let rawValue: UInt64

    init(_ bytes: ArraySlice<UInt8>) {
        rawValue = bytes.reversed().reduce(0) { $0 << 8 | UInt64($1) }
    }

    /// Forme affichée par minisign, en 16 chiffres hexadécimaux majuscules : `7187B435F3714F09`.
    public var description: String {
        let hex = String(rawValue, radix: 16, uppercase: true)
        return String(repeating: "0", count: 16 - hex.count) + hex
    }
}

/// Clé publique d'une app, au format de minisign et de Tauri.
public struct MinisignPublicKey: Hashable, Sendable {
    public let keyID: MinisignKeyID
    /// Clé Ed25519 brute (32 octets). CryptoKit ne la lit qu'au moment de vérifier.
    let ed25519: [UInt8]

    /// Lit une clé au format Tauri : base64 standard du texte complet de la clé minisign, comme
    /// la valeur de `plugins.updater.pubkey` ou une entrée de `DrakBasePublicKeys`.
    public init(tauri encoded: String) throws(SignatureError) {
        try self.init(minisignText: MinisignText.decodeTauri(encoded))
    }

    /// Lit le texte minisign d'une clé publique : le commentaire non signé (`untrusted comment:`),
    /// puis la ligne de données `base64("Ed" ‖ key_id ‖ clé Ed25519)`. Une app n'embarque que le
    /// format Tauri : cette lecture sert aux jeux d'essai du paquet.
    package init(minisign text: String) throws(SignatureError) {
        try self.init(minisignText: Array(text.utf8))
    }

    private init(minisignText text: [UInt8]) throws(SignatureError) {
        let lines = MinisignText.lines(text)
        guard lines.count >= 2, lines[0].starts(with: MinisignText.untrustedCommentPrefix),
              let data = StrictBase64.decode(Array(lines[1])),
              data.count == 2 + 8 + 32,
              data[0] == UInt8(ascii: "E"), data[1] == UInt8(ascii: "d")
        else {
            throw .malformedPublicKey
        }
        keyID = MinisignKeyID(data[2..<10])
        ed25519 = Array(data[10...])
    }

    /// Contrôle Ed25519 avec CryptoKit. Une clé que CryptoKit refuse ne valide rien.
    func isValidSignature(_ signature: [UInt8], for message: [UInt8]) -> Bool {
        guard let key = try? Curve25519.Signing.PublicKey(rawRepresentation: ed25519) else { return false }
        return key.isValidSignature(signature, for: message)
    }
}
