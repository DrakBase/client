import AppsMarketCrypto
import AppsMarketTestSupport
import Foundation
import Testing

@Suite("Clé principale et clé de secours")
struct KeyringTests {
    let testApp: MinisignPublicKey
    let otherApp: MinisignPublicKey
    let payload: Data

    init() throws {
        testApp = try MinisignPublicKey(tauri: Fixtures.text("keys/test-app.pub"))
        otherApp = try MinisignPublicKey(tauri: Fixtures.text("keys/other-app.pub"))
        payload = try Fixtures.data("files/payload.bin")
    }

    @Test("La signature est vérifiée avec la clé dont l'identifiant est le sien, principale ou de secours")
    func keySelection() throws {
        let signature = try MinisignSignature(tauri: Fixtures.text("files/payload.bin.sig"))
        let foreign = try MinisignSignature(tauri: Fixtures.text("files/payload.bin.other-key.sig"))

        let primaryFirst = MinisignKeyring(primary: testApp, backups: [otherApp])
        #expect(try primaryFirst.key(for: signature) == testApp)
        #expect(try primaryFirst.verify(payload, signature: signature).keyID == testApp.keyID)

        // Signée par la clé de secours : `other-app` signe le même fichier.
        let backupSigned = MinisignKeyring(primary: otherApp, backups: [testApp])
        #expect(try backupSigned.key(for: signature) == testApp)
        #expect(try backupSigned.key(for: foreign) == otherApp)
        #expect(backupSigned.keys == [otherApp, testApp])
    }

    @Test("Aucune clé ne correspond : key_id_mismatch, la clé principale étant celle attendue")
    func unknownKey() throws {
        let foreign = try MinisignSignature(tauri: Fixtures.text("files/payload.bin.other-key.sig"))
        let keyring = MinisignKeyring(primary: testApp)
        #expect(throws: SignatureError.keyIDMismatch(expected: testApp.keyID, found: otherApp.keyID)) {
            try keyring.key(for: foreign)
        }
        #expect(throws: SignatureError.keyIDMismatch(expected: testApp.keyID, found: otherApp.keyID)) {
            try keyring.verify(payload, signature: foreign)
        }
    }

    @Test("Le format historique est refusé avant le choix de la clé, comme dans l'ordre du protocole")
    func legacyFirst() throws {
        let legacy = try MinisignSignature(tauri: Fixtures.text("files/payload.bin.legacy-alg.sig"))
        #expect(throws: SignatureError.legacy) { try MinisignKeyring(primary: otherApp).key(for: legacy) }
    }

    @Test("Vérification d'un fichier avec le trousseau, hors de l'acteur de l'appelant")
    func fileVerification() async throws {
        let signature = try MinisignSignature(tauri: Fixtures.text("files/payload.bin.sig"))
        let keyring = MinisignKeyring(primary: otherApp, backups: [testApp])
        let verified = try await keyring.verifyFile(at: Fixtures.url("files/payload.bin"), signature: signature, chunkByteCount: 4096)
        try verified.requireVersion("1.2.0")
        await #expect(throws: SignatureError.invalid) {
            try await keyring.verifyFile(at: Fixtures.url("files/payload-tampered.bin"), signature: signature)
        }
    }
}
