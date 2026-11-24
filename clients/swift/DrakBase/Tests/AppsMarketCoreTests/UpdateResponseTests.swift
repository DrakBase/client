import AppsMarketCore
import AppsMarketCrypto
import AppsMarketTestSupport
import Foundation
import Testing

@Suite("Réponse du serveur")
struct UpdateResponseTests {
    @Test("Les cas de fixtures/update-response.json donnent exactement le résultat attendu")
    func fixtureCases() throws {
        let fixtures = try Fixtures.updateResponses()
        #expect(fixtures.contains { $0.isAccepted })
        #expect(fixtures.contains { !$0.isAccepted })
        for fixture in fixtures {
            guard fixture.isAccepted else {
                #expect(throws: DecodingError.self, "cas « \(fixture.name) »") {
                    try JSONDecoder().decode(UpdateResponse.self, from: fixture.body)
                }
                continue
            }
            let response: UpdateResponse
            do {
                response = try JSONDecoder().decode(UpdateResponse.self, from: fixture.body)
            } catch {
                Issue.record("cas « \(fixture.name) » : \(error)")
                continue
            }
            // Ce qui a été lu, champ par champ, les champs absents valant `null`.
            let read: [String: Any] = [
                "version": response.version.description,
                "url": response.url.absoluteString,
                "signature": response.signature,
                "notes": response.notes.map { $0 as Any } ?? NSNull(),
                "pub_date": response.pubDate,
                "size": NSNumber(value: response.size),
                "sha256": response.sha256,
                "min_os_version": response.minOSVersion.map { $0 as Any } ?? NSNull(),
                "channel": response.channel.rawValue,
            ]
            #expect(Set(read.keys) == Set(UpdateResponseFixture.fields))
            #expect(NSDictionary(dictionary: read).isEqual(to: fixture.check), "cas « \(fixture.name) » : lu \(read)")
        }
    }

    @Test("Réponse, signature et archive du jeu d'essai : la vérification complète passe")
    func verificationWithResponse() throws {
        var body = try Fixtures.completeUpdateResponse()
        body["version"] = "1.2"
        body["signature"] = try Fixtures.text("files/payload.bin.sig").trimmingCharacters(in: .whitespacesAndNewlines)
        let response = try JSONDecoder().decode(UpdateResponse.self, from: JSONSerialization.data(withJSONObject: body))
        let key = try MinisignPublicKey(tauri: Fixtures.text("keys/test-app.pub"))
        let verified = try key.verify(Fixtures.data("files/payload.bin"), signature: MinisignSignature(tauri: response.signature))
        // La version annoncée, normalisée en 1.2.0, est celle qui est signée.
        try verified.requireVersion(response.version)

        let other = try SemanticVersion.parseRelease("1.3")
        #expect(throws: SignatureError.versionMismatch(signed: "1.2.0", expected: "1.3.0")) {
            try verified.requireVersion(other)
        }
    }
}
