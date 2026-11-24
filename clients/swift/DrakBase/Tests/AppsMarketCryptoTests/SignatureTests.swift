import AppsMarketCrypto
import AppsMarketTestSupport
import Foundation
import Testing

@Suite("Vérification des signatures")
struct SignatureTests {
    let key: MinisignPublicKey
    let signature: MinisignSignature
    let payload: Data

    init() throws {
        key = try MinisignPublicKey(tauri: Fixtures.text("keys/test-app.pub"))
        signature = try MinisignSignature(tauri: Fixtures.text("files/payload.bin.sig"))
        payload = try Fixtures.data("files/payload.bin")
    }

    @Test("Les cas de fixtures/signatures.json donnent exactement le résultat attendu")
    func fixtureCases() throws {
        let fixtures = try Fixtures.signatures()
        // Chaque code du contrat est couvert par au moins un cas.
        #expect(Set(fixtures.cases.map(\.expected)) == Set(fixtures.expectedCodes))
        for testCase in fixtures.cases {
            let result = try outcome(of: testCase)
            #expect(result == testCase.expected, "cas « \(testCase.name) »")
        }
    }

    @Test("Le fichier de 65 537 octets vérifié en flux, par morceaux, comme en mémoire")
    func streamingByChunks() throws {
        let inMemory = try key.verify(payload, signature: signature)
        for chunkSize in [1, 100, 128, 4096, 65_536, payload.count] {
            var verifier = try key.verifier(for: signature)
            for start in stride(from: 0, to: payload.count, by: chunkSize) {
                verifier.update(data: payload[start..<min(start + chunkSize, payload.count)])
            }
            #expect(try verifier.finalize() == inMemory, "morceaux de \(chunkSize) octets")
        }
        #expect(inMemory.keyID == key.keyID)
        #expect(inMemory.field("file") == "payload.bin")
        #expect(inMemory.field("version") == "1.2.0")
        try inMemory.requireVersion("1.2.0")
    }

    @Test("Un fichier modifié est refusé aussi en flux")
    func tamperedStream() throws {
        let tampered = try Fixtures.data("files/payload-tampered.bin")
        var verifier = try key.verifier(for: signature)
        for start in stride(from: 0, to: tampered.count, by: 4096) {
            verifier.update(data: tampered[start..<min(start + 4096, tampered.count)])
        }
        #expect(throws: SignatureError.invalid) { try verifier.finalize() }
    }

    @Test("Format historique et autre clé sont refusés avant de lire la moindre donnée")
    func refusedBeforeReading() throws {
        let legacy = try MinisignSignature(tauri: Fixtures.text("files/payload.bin.legacy-alg.sig"))
        #expect(!legacy.isPrehashed)
        #expect(signature.isPrehashed)
        #expect(throws: SignatureError.legacy) { try key.verifier(for: legacy) }

        let otherKey = try MinisignPublicKey(tauri: Fixtures.text("keys/other-app.pub"))
        let foreign = try MinisignSignature(tauri: Fixtures.text("files/payload.bin.other-key.sig"))
        #expect(foreign.keyID == otherKey.keyID)
        #expect(throws: SignatureError.keyIDMismatch(expected: key.keyID, found: otherKey.keyID)) {
            try key.verifier(for: foreign)
        }
    }

    @Test("La version signée est comparée exactement, après les signatures")
    func signedVersion() throws {
        let verified = try key.verify(payload, signature: signature)
        #expect(throws: SignatureError.versionMismatch(signed: "1.2.0", expected: "1.3.0")) {
            try verified.requireVersion("1.3.0")
        }
        // La version annoncée doit déjà être canonique : « 1.2 » n'est pas « 1.2.0 ».
        #expect(throws: SignatureError.versionMismatch(signed: "1.2.0", expected: "1.2")) {
            try verified.requireVersion("1.2")
        }

        let noVersion = try MinisignSignature(tauri: Fixtures.text("files/payload.bin.no-version.sig"))
        let unversioned = try key.verify(payload, signature: noVersion)
        #expect(unversioned.field("version") == nil)
        #expect(unversioned.field("file") == "payload.bin")
        #expect(throws: SignatureError.missingVersion) { try unversioned.requireVersion("1.2.0") }
    }

    @Test("Un fichier sur disque est vérifié par morceaux, hors de l'acteur de l'appelant")
    func fileVerification() async throws {
        let verified = try await key.verifyFile(at: Fixtures.url("files/payload.bin"), signature: signature)
        #expect(verified == (try key.verify(payload, signature: signature)))

        let small = try await key.verifyFile(
            at: Fixtures.url("files/payload.bin"), signature: signature, chunkByteCount: 4096
        )
        #expect(small == verified)

        await #expect(throws: SignatureError.invalid) {
            try await key.verifyFile(at: Fixtures.url("files/payload-tampered.bin"), signature: signature)
        }
    }

    @Test("Le format et la clé sont contrôlés avant même d'ouvrir le fichier")
    func fileChecksOrder() async throws {
        let missing = Fixtures.url("files/absent.bin")
        let legacy = try MinisignSignature(tauri: Fixtures.text("files/payload.bin.legacy-alg.sig"))
        await #expect(throws: SignatureError.legacy) {
            try await key.verifyFile(at: missing, signature: legacy)
        }
        // Sans ces refus, un fichier absent donne l'erreur de lecture, pas une erreur de signature.
        let error = await #expect(throws: (any Error).self) {
            try await key.verifyFile(at: missing, signature: signature)
        }
        #expect(!(error is SignatureError))
    }

    @Test("Une vérification de fichier s'arrête si la tâche est annulée")
    func cancellation() async throws {
        let key = key
        let signature = signature
        let error = await withTaskGroup(of: (any Error)?.self) { group in
            // Une tâche ajoutée à un groupe annulé naît annulée.
            group.cancelAll()
            group.addTask {
                do {
                    _ = try await key.verifyFile(
                        at: Fixtures.url("files/payload.bin"), signature: signature, chunkByteCount: 4096
                    )
                    return nil
                } catch {
                    return error
                }
            }
            return await group.next() ?? nil
        }
        #expect(error is CancellationError)
    }

    /// Code du jeu d'essai pour un cas : les mêmes étapes que le test Rust
    /// (`crates/am-protocol/tests/fixtures.rs`). Les versions annoncées y sont déjà canoniques.
    private func outcome(of testCase: SignatureFixtures.Case) throws -> String {
        let key = try MinisignPublicKey(tauri: Fixtures.text(testCase.key))
        let signature = try MinisignSignature(tauri: Fixtures.text(testCase.signature))
        let data = try Fixtures.data(testCase.file)
        do {
            try key.verify(data, signature: signature).requireVersion(testCase.announcedVersion)
            return "ok"
        } catch {
            switch error {
            case .legacy: return "legacy"
            case .keyIDMismatch: return "key_id_mismatch"
            case .invalid: return "invalid"
            case .missingVersion: return "missing_version"
            case .versionMismatch: return "version_mismatch"
            case .base64, .utf8, .malformedPublicKey, .malformedSignature: return "inattendu : \(error)"
            }
        }
    }
}
