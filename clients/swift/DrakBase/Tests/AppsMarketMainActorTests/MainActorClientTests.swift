import DrakBase
import AppsMarketTestSupport
import Foundation
import Testing

/// Ce module est compilé comme Translate et Pli : isolation MainActor par défaut et concurrence
/// « accessible », avertissements refusés. Tout ce qui suit doit donc compiler sans le moindre
/// avertissement : c'est la preuve que l'API publique s'utilise depuis ce genre de code.
@Suite("Depuis du code isolé sur le MainActor")
struct MainActorClientTests {
    @Test("Vérification complète lancée depuis le MainActor : adresse, réponse, signature, version")
    func fullCheck() async throws {
        MainActor.preconditionIsolated("ce module doit être isolé sur le MainActor par défaut")

        let feed = try UpdateFeed(infoDictionary: [UpdateFeed.infoPlistKey: "https://maj.example.fr/api/v1/update/translate"])
        let current = try SemanticVersion.parseClient("1.1.0+42")
        let url = feed.checkURL(currentVersion: current, channel: .stable)
        #expect(url.path() == "/api/v1/update/translate/darwin/\(CPUArchitecture.current.rawValue)/1.1.0")

        let signature = try Fixtures.text("files/payload.bin.sig").trimmingCharacters(in: .whitespacesAndNewlines)
        // La réponse du jeu d'essai commun (version 1.2.0), avec la signature de l'archive d'essai.
        var body = try Fixtures.completeUpdateResponse()
        body["signature"] = signature
        let response = try JSONDecoder().decode(UpdateResponse.self, from: JSONSerialization.data(withJSONObject: body))
        #expect(response.version > current)

        let key = try MinisignPublicKey(tauri: Fixtures.text("keys/test-app.pub"))
        // `verifyFile` est `@concurrent` : le hachage tourne hors du MainActor, qui reste libre.
        let verified = try await key.verifyFile(
            at: Fixtures.url("files/payload.bin"),
            signature: MinisignSignature(tauri: response.signature),
            chunkByteCount: 4096
        )
        try verified.requireVersion(response.version)
        #expect(verified.keyID == key.keyID)
    }

    @Test("Les types du paquet passent dans une fonction @concurrent de l'app")
    func appConcurrentFunction() async throws {
        let key = try MinisignPublicKey(tauri: Fixtures.text("keys/test-app.pub"))
        let signature = try MinisignSignature(tauri: Fixtures.text("files/payload.bin.sig"))
        let verifier = try key.verifier(for: signature)
        let verified = try await Self.feed(verifier, with: Fixtures.data("files/payload.bin"))
        #expect(verified.field("version") == "1.2.0")

        let digest = await Self.hash(Fixtures.pattern(length: 1000))
        #expect(digest == BLAKE2b512.hash(data: Fixtures.pattern(length: 1000)))
    }

    /// Ce qu'une app ferait pour un calcul à elle : une fonction non isolée qui reçoit les valeurs
    /// `Sendable` du paquet.
    @concurrent
    nonisolated static func feed(_ verifier: SignatureVerifier, with data: Data) async throws -> VerifiedSignature {
        var verifier = verifier
        for start in stride(from: 0, to: data.count, by: 4096) {
            verifier.update(data: data[start..<min(start + 4096, data.count)])
        }
        return try verifier.finalize()
    }

    @concurrent
    nonisolated static func hash(_ bytes: [UInt8]) async -> [UInt8] {
        var hasher = BLAKE2b512()
        hasher.update(data: bytes)
        return hasher.finalize()
    }
}
