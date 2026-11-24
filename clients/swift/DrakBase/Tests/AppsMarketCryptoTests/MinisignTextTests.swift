@testable import AppsMarketCrypto
import AppsMarketTestSupport
import Foundation
import Testing

/// Lecture des clés et des signatures : on accepte exactement ce qu'accepte l'implémentation Rust.
@Suite("Lecture des textes minisign")
struct MinisignTextTests {
    /// Texte minisign d'un fichier du jeu d'essai, sorti de son enveloppe Tauri.
    static func minisignText(_ relativePath: String) throws -> String {
        let data = try #require(Data(base64Encoded: try Fixtures.text(relativePath)))
        return try #require(String(data: data, encoding: .utf8))
    }

    /// Remet un texte minisign dans une enveloppe Tauri.
    static func tauri(_ text: String) -> String {
        Data(text.utf8).base64EncodedString()
    }

    /// Lignes du texte, sans les fins de ligne.
    static func lines(_ relativePath: String) throws -> [String] {
        try minisignText(relativePath).split(separator: "\n", omittingEmptySubsequences: false).map(String.init)
    }

    @Test("Ce qui n'est pas du base64 ou de l'UTF-8 est refusé sans planter")
    func garbage() {
        #expect(throws: SignatureError.base64) { try MinisignPublicKey(tauri: "pas du base64 !") }
        #expect(throws: SignatureError.base64) { try MinisignSignature(tauri: "QUJD=") }
        #expect(throws: SignatureError.utf8) { try MinisignSignature(tauri: Data([0xFF, 0xFE, 0x0A]).base64EncodedString()) }
        #expect(throws: SignatureError.malformedSignature) {
            try MinisignSignature(minisign: "untrusted comment: x\nQUJD\n")
        }
        #expect(throws: SignatureError.malformedPublicKey) { try MinisignPublicKey(minisign: "") }
        #expect(throws: SignatureError.malformedSignature) { try MinisignSignature(tauri: "") }
    }

    @Test("Les blancs autour de l'enveloppe Tauri sont ignorés")
    func envelopeWhitespace() throws {
        let encoded = try Fixtures.text("keys/test-app.pub")
        let key = try MinisignPublicKey(tauri: encoded)
        #expect(try MinisignPublicKey(tauri: " \n\(encoded)\r\n\t") == key)
        #expect(try MinisignPublicKey(tauri: "\u{00A0}\(encoded)\u{2028}") == key)
        #expect(throws: SignatureError.base64) { try MinisignPublicKey(tauri: String(encoded.dropLast(2))) }
    }

    @Test("L'identifiant de clé s'affiche comme dans le commentaire de minisign")
    func keyIDDescription() throws {
        for name in ["test-app", "other-app"] {
            let lines = try Self.lines("keys/\(name).pub")
            let key = try MinisignPublicKey(minisign: try Self.minisignText("keys/\(name).pub"))
            #expect(lines[0] == "untrusted comment: minisign public key: \(key.keyID)")
            #expect(key.keyID.description.count == 16)
        }
        let id = MinisignKeyID([1, 2, 3, 4, 5, 6, 7, 8])
        #expect(id.description == "0807060504030201")
        #expect(MinisignKeyID([0, 0, 0, 0, 0, 0, 0, 1]).description == "0100000000000000")
    }

    @Test("Les cas de lecture de fixtures/signatures.json donnent exactement le résultat attendu")
    func fixtureParsingCases() throws {
        let fixtures = try Fixtures.signatures()
        // Chaque résultat possible est couvert, pour les clés comme pour les signatures.
        #expect(Set(fixtures.parsing.map(\.expected)) == Set(fixtures.parsingCodes))
        #expect(Set(fixtures.parsing.map(\.kind)) == ["public_key", "signature"])
        for parsingCase in fixtures.parsing {
            let encoded = try Fixtures.text(parsingCase.file)
            switch (parsingCase.kind, parsingCase.expected) {
            case ("public_key", "ok"):
                let key = try MinisignPublicKey(tauri: encoded)
                #expect(key.keyID.description == parsingCase.keyId, "cas « \(parsingCase.name) »")
            case ("public_key", "malformed"):
                #expect(throws: SignatureError.malformedPublicKey, "cas « \(parsingCase.name) »") {
                    try MinisignPublicKey(tauri: encoded)
                }
            case ("signature", "ok"):
                let signature = try MinisignSignature(tauri: encoded)
                #expect(signature.keyID.description == parsingCase.keyId, "cas « \(parsingCase.name) »")
            case ("signature", "malformed"):
                #expect(throws: SignatureError.malformedSignature, "cas « \(parsingCase.name) »") {
                    try MinisignSignature(tauri: encoded)
                }
            default:
                Issue.record("cas « \(parsingCase.name) » : type ou résultat inconnu")
            }
        }
    }

    @Test("Clé publique : commentaire, puis données « Ed » de 42 octets")
    func publicKeyFormat() throws {
        let lines = try Self.lines("keys/test-app.pub")
        let key = try MinisignPublicKey(minisign: lines.joined(separator: "\n"))
        // Fins de ligne Windows et lignes en plus après les données.
        #expect(try MinisignPublicKey(minisign: lines.joined(separator: "\r\n")) == key)
        #expect(try MinisignPublicKey(minisign: lines[0] + "\n" + lines[1] + "\nautre chose\n") == key)

        #expect(throws: SignatureError.malformedPublicKey) { try MinisignPublicKey(minisign: lines[1]) }
        #expect(throws: SignatureError.malformedPublicKey) { try MinisignPublicKey(minisign: "\n" + lines[1] + "\n") }
        #expect(throws: SignatureError.malformedPublicKey) { try MinisignPublicKey(minisign: " \t\n" + lines[1]) }
        // La première ligne doit être le commentaire non signé, pas seulement une ligne non vide.
        #expect(throws: SignatureError.malformedPublicKey) { try MinisignPublicKey(minisign: "commentaire\n" + lines[1]) }
        #expect(throws: SignatureError.malformedPublicKey) {
            try MinisignPublicKey(minisign: lines[0] + "\n" + lines[1] + " \n")
        }

        // Algorithme « ED » ou longueur fausse.
        var data = try #require(Data(base64Encoded: lines[1]))
        data[1] = UInt8(ascii: "D")
        #expect(throws: SignatureError.malformedPublicKey) {
            try MinisignPublicKey(minisign: lines[0] + "\n" + data.base64EncodedString())
        }
        data[1] = UInt8(ascii: "d")
        #expect(try MinisignPublicKey(minisign: lines[0] + "\n" + data.base64EncodedString()) == key)
        #expect(throws: SignatureError.malformedPublicKey) {
            try MinisignPublicKey(minisign: lines[0] + "\n" + data.dropLast().base64EncodedString())
        }
    }

    @Test("Signature : quatre lignes, base64 strict, préfixe du commentaire de confiance")
    func signatureFormat() throws {
        let lines = try Self.lines("files/payload.bin.sig")
        let key = try MinisignPublicKey(tauri: Fixtures.text("keys/test-app.pub"))
        let payload = try Fixtures.data("files/payload.bin")
        let signature = try MinisignSignature(minisign: lines.joined(separator: "\n"))
        let verified = try key.verify(payload, signature: signature)

        func parse(_ lines: [String]) throws(SignatureError) -> MinisignSignature {
            try MinisignSignature(minisign: lines.joined(separator: "\n"))
        }
        func replacing(_ index: Int, with line: String) -> [String] {
            var copy = lines
            copy[index] = line
            return copy
        }

        // Fins de ligne Windows : même signature, toujours valide.
        let crlf = try MinisignSignature(minisign: lines.joined(separator: "\r\n"))
        #expect(crlf == signature)
        #expect(try key.verify(payload, signature: crlf) == verified)

        // Ligne de données avec un blanc, sans remplissage, ou avec des bits de fin non nuls : 74 octets
        // finissent par `xxx=`, dont les deux bits de poids faible du dernier caractère ne servent pas.
        #expect(throws: SignatureError.malformedSignature) { try parse(replacing(1, with: lines[1] + " ")) }
        #expect(throws: SignatureError.malformedSignature) { try parse(replacing(1, with: String(lines[1].dropLast()))) }
        let alphabet = Array("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/")
        var characters = Array(lines[1])
        #expect(characters.last == "=")
        let last = characters.count - 2
        characters[last] = alphabet[try #require(alphabet.firstIndex(of: characters[last])) ^ 1]
        #expect(throws: SignatureError.malformedSignature) { try parse(replacing(1, with: String(characters))) }
        #expect(throws: SignatureError.malformedSignature) { try parse(replacing(3, with: lines[3] + "=")) }

        // Préfixe du commentaire de confiance, ligne de commentaire et nombre de lignes.
        #expect(throws: SignatureError.malformedSignature) {
            try parse(replacing(2, with: lines[2].replacingOccurrences(of: "trusted comment: ", with: "trusted comment:")))
        }
        #expect(throws: SignatureError.malformedSignature) { try parse(replacing(0, with: "  ")) }
        #expect(throws: SignatureError.malformedSignature) {
            try parse(replacing(0, with: "signature from tauri secret key"))
        }
        #expect(throws: SignatureError.malformedSignature) { try parse(Array(lines.prefix(3))) }

        // Algorithme inconnu.
        var data = try #require(Data(base64Encoded: lines[1]))
        data[0] = UInt8(ascii: "X")
        #expect(throws: SignatureError.malformedSignature) { try parse(replacing(1, with: data.base64EncodedString())) }
    }

    @Test("Le commentaire de confiance n'est rendu qu'avec ses octets exacts")
    func trustedCommentBytes() throws {
        let key = try MinisignPublicKey(tauri: Fixtures.text("keys/test-app.pub"))
        let payload = try Fixtures.data("files/payload.bin")
        var lines = try Self.lines("files/payload.bin.sig")
        lines[2] += " "
        let signature = try MinisignSignature(minisign: lines.joined(separator: "\n"))
        #expect(throws: SignatureError.invalid) { try key.verify(payload, signature: signature) }
    }

    @Test("Champs « clé:valeur » du commentaire de confiance")
    func trustedCommentFields() {
        let verified = VerifiedSignature(
            keyID: MinisignKeyID([0, 0, 0, 0, 0, 0, 0, 0]),
            trustedComment: "timestamp:1767225600\tfile:a.app.tar.gz\tversion:1.2.0\tnote:a:b\t\tsans-deux-points\tversion:9.9.9"
        )
        #expect(verified.field("timestamp") == "1767225600")
        #expect(verified.field("file") == "a.app.tar.gz")
        // Le nom s'arrête au premier « : », et le premier champ de ce nom l'emporte.
        #expect(verified.field("note") == "a:b")
        #expect(verified.field("version") == "1.2.0")
        #expect(verified.field("sans-deux-points") == nil)
        #expect(verified.field("absent") == nil)
        #expect(throws: Never.self) { try verified.requireVersion("1.2.0") }
        #expect(throws: SignatureError.versionMismatch(signed: "1.2.0", expected: "1.3.0")) {
            try verified.requireVersion("1.3.0")
        }

        let empty = VerifiedSignature(keyID: MinisignKeyID([0, 0, 0, 0, 0, 0, 0, 0]), trustedComment: "")
        #expect(empty.field("version") == nil)
        #expect(throws: SignatureError.missingVersion) { try empty.requireVersion("1.2.0") }
    }

    @Test("Les messages d'erreur sont ceux de l'implémentation Rust")
    func errorMessages() {
        let expected = MinisignKeyID([9, 0x4F, 0x71, 0xF3, 0x35, 0xB4, 0x87, 0x71])
        let found = MinisignKeyID([0x2E, 0xE4, 0xA5, 0x0C, 0x03, 0xB0, 0x18, 0x71])
        #expect(SignatureError.keyIDMismatch(expected: expected, found: found).localizedDescription
            == "signature faite avec la clé 7118B0030CA5E42E, attendue 7187B435F3714F09")
        #expect(SignatureError.versionMismatch(signed: "1.2.0", expected: "1.3.0").localizedDescription
            == "version signée « 1.2.0 », attendue « 1.3.0 »")
        #expect(SignatureError.legacy.localizedDescription == "signature au format historique non préhaché : refusée")
    }
}
