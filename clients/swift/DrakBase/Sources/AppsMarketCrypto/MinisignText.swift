/// Lecture des textes minisign et de leur enveloppe Tauri, avec les règles exactes de
/// `am_protocol::signature` côté Rust : ce qu'une implémentation refuse, l'autre le refuse aussi.
enum MinisignText {
    /// Décode l'enveloppe Tauri (base64 standard du texte minisign), en ignorant les blancs autour,
    /// et rend le texte en UTF-8.
    static func decodeTauri(_ encoded: String) throws(SignatureError) -> [UInt8] {
        guard let text = StrictBase64.decode(Array(encoded.trimmingUnicodeWhitespace().utf8)) else {
            throw .base64
        }
        let invalidUTF8 = transcode(
            text.makeIterator(), from: UTF8.self, to: UTF32.self, stoppingOnError: true, into: { _ in }
        )
        guard !invalidUTF8 else { throw .utf8 }
        return text
    }

    /// Lignes d'un texte, découpées comme `str::lines` en Rust : sur `\n`, en retirant le `\r` d'une
    /// fin de ligne `\r\n`. Le découpage se fait sur les octets, car Swift voit `\r\n` comme un seul
    /// caractère.
    static func lines(_ text: [UInt8]) -> [ArraySlice<UInt8>] {
        var lines: [ArraySlice<UInt8>] = []
        var start = text.startIndex
        for index in text.indices where text[index] == UInt8(ascii: "\n") {
            var end = index
            if end > start, text[end - 1] == UInt8(ascii: "\r") {
                end -= 1
            }
            lines.append(text[start..<end])
            start = index + 1
        }
        if start < text.endIndex {
            lines.append(text[start...])
        }
        return lines
    }

    /// Début obligatoire de la première ligne, le commentaire non signé. La ligne de données est
    /// toujours la deuxième ligne telle quelle : un texte décalé par une ligne vide est refusé, au
    /// lieu de prendre l'identifiant de clé sur une ligne et la clé sur une autre.
    static let untrustedCommentPrefix = Array("untrusted comment:".utf8)
}

extension String {
    /// Sans les blancs Unicode de début et de fin, comme `str::trim` en Rust. Sert à lire
    /// l'enveloppe Tauri ici, et les versions dans `AppsMarketCore`.
    package func trimmingUnicodeWhitespace() -> String {
        let scalars = unicodeScalars
        guard let first = scalars.firstIndex(where: { !$0.properties.isWhitespace }),
              let last = scalars.lastIndex(where: { !$0.properties.isWhitespace })
        else {
            return ""
        }
        return String(Substring(scalars[first...last]))
    }
}

/// Base64 standard strict, comme le moteur `STANDARD` de la crate `base64` : remplissage `=`
/// canonique obligatoire, bits de fin nuls, et aucun autre caractère, pas même un blanc.
enum StrictBase64 {
    static func decode(_ encoded: [UInt8]) -> [UInt8]? {
        guard encoded.count % 4 == 0 else { return nil }
        var decoded: [UInt8] = []
        decoded.reserveCapacity(encoded.count / 4 * 3)
        let pad = UInt8(ascii: "=")
        for start in stride(from: 0, to: encoded.count, by: 4) {
            // Le remplissage n'est permis qu'à la fin : `xx==` ou `xxx=`. Un `=` ailleurs est refusé
            // plus bas, comme tout caractère hors de l'alphabet.
            var padding = 0
            if start + 4 == encoded.count, encoded[start + 3] == pad {
                padding = encoded[start + 2] == pad ? 2 : 1
            }
            var group: UInt32 = 0
            for offset in 0..<(4 - padding) {
                guard let value = sextet(encoded[start + offset]) else { return nil }
                group = group << 6 | UInt32(value)
            }
            group <<= 6 * UInt32(padding)
            // Les bits qui ne tombent dans aucun octet doivent être nuls.
            if padding == 2, group & 0xFFFF != 0 { return nil }
            if padding == 1, group & 0xFF != 0 { return nil }
            decoded.append(UInt8(truncatingIfNeeded: group >> 16))
            if padding < 2 {
                decoded.append(UInt8(truncatingIfNeeded: group >> 8))
            }
            if padding < 1 {
                decoded.append(UInt8(truncatingIfNeeded: group))
            }
        }
        return decoded
    }

    private static func sextet(_ character: UInt8) -> UInt8? {
        switch character {
        case UInt8(ascii: "A")...UInt8(ascii: "Z"): character - UInt8(ascii: "A")
        case UInt8(ascii: "a")...UInt8(ascii: "z"): character - UInt8(ascii: "a") + 26
        case UInt8(ascii: "0")...UInt8(ascii: "9"): character - UInt8(ascii: "0") + 52
        case UInt8(ascii: "+"): 62
        case UInt8(ascii: "/"): 63
        default: nil
        }
    }
}
