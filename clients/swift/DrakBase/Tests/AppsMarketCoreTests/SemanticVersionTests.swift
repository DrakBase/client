import AppsMarketCore
import AppsMarketTestSupport
import Foundation
import Testing

@Suite("Versions")
struct SemanticVersionTests {
    @Test("Normalisation de fixtures/versions.json, pour une publication et pour une app")
    func fixtureNormalization() throws {
        let fixtures = try Fixtures.versions()
        #expect(!fixtures.normalize.isEmpty)
        for testCase in fixtures.normalize {
            let release = try? SemanticVersion.parseRelease(testCase.input).description
            let client = try? SemanticVersion.parseClient(testCase.input).description
            #expect(release == testCase.release, "publication « \(testCase.input) »")
            #expect(client == testCase.client, "client « \(testCase.input) »")
        }
    }

    @Test("Ordre de fixtures/versions.json")
    func fixtureOrdering() throws {
        let ordered = try Fixtures.versions().ordered
        let versions = try ordered.map { try SemanticVersion.parseRelease($0) }
        #expect(versions.map(\.description) == ordered)
        for (index, lower) in versions.enumerated() {
            for higher in versions[(index + 1)...] {
                #expect(lower < higher, "\(lower) devrait précéder \(higher)")
                #expect(!(higher < lower), "\(higher) ne devrait pas précéder \(lower)")
                #expect(lower != higher)
            }
        }
        #expect(versions.reversed().sorted() == versions)
    }

    @Test("Préfixe, composants manquants et pré-version")
    func lenientInput() throws {
        #expect(try SemanticVersion.parseRelease(" v1.2 ").description == "1.2.0")
        #expect(try SemanticVersion.parseRelease("3").description == "3.0.0")
        #expect(try SemanticVersion.parseRelease("1.2-beta.1").description == "1.2.0-beta.1")
        #expect(try SemanticVersion.parseRelease("\u{00A0}V1.2.3\n").description == "1.2.3")
        // Le cœur numérique s'arrête au premier « - » : comme côté Rust, « 1-2.3 » est une pré-version.
        #expect(try SemanticVersion.parseRelease("1-2.3").description == "1.0.0-2.3")
        #expect(try SemanticVersion.parseRelease("v1.2") == SemanticVersion.parseRelease("1.2.0"))
    }

    @Test("Les métadonnées de build sont refusées pour une publication, ignorées pour une app")
    func buildMetadata() throws {
        #expect(throws: VersionError.buildMetadata("1.2.3+7")) { try SemanticVersion.parseRelease("1.2.3+7") }
        #expect(throws: VersionError.buildMetadata("v1.2+7")) { try SemanticVersion.parseRelease(" v1.2+7 ") }
        #expect(try SemanticVersion.parseClient("1.2.3+7").description == "1.2.3")
        #expect(try SemanticVersion.parseClient("1.0.0-rc.1+001.b-c").description == "1.0.0-rc.1")
        #expect(throws: VersionError.invalid("1.2.3+")) { try SemanticVersion.parseClient("1.2.3+") }
        #expect(throws: VersionError.invalid("1.2.3+a_b")) { try SemanticVersion.parseClient("1.2.3+a_b") }
    }

    @Test("Chaque refus a son erreur, comme côté Rust")
    func errors() {
        #expect(throws: VersionError.empty) { try SemanticVersion.parseRelease("  ") }
        #expect(throws: VersionError.empty) { try SemanticVersion.parseClient("V") }
        #expect(throws: VersionError.tooManyComponents("1.2.3.4")) { try SemanticVersion.parseClient(" 1.2.3.4 ") }
        #expect(throws: VersionError.tooManyComponents("1.2.3.")) { try SemanticVersion.parseClient("1.2.3.") }
        #expect(throws: VersionError.invalid("latest")) { try SemanticVersion.parseRelease("latest") }
        #expect(throws: VersionError.invalid("vv1")) { try SemanticVersion.parseRelease("vv1") }
        #expect(throws: VersionError.invalid("v 1.2")) { try SemanticVersion.parseRelease("v 1.2") }
        #expect(throws: VersionError.invalid("1..")) { try SemanticVersion.parseRelease("1..") }
        #expect(throws: VersionError.invalid("1.2.3-")) { try SemanticVersion.parseClient("1.2.3-") }
        #expect(VersionError.buildMetadata("1.2.3+7").localizedDescription
            == "version « 1.2.3+7 » : les métadonnées de build (+…) sont interdites")
    }

    @Test("Règles strictes de SemVer, comme la crate semver")
    func strictRules() throws {
        #expect(try SemanticVersion.parseRelease("0.0.0").description == "0.0.0")
        #expect(try SemanticVersion.parseRelease("18446744073709551615.0.0").major == .max)
        #expect(throws: VersionError.invalid("18446744073709551616")) {
            try SemanticVersion.parseRelease("18446744073709551616")
        }
        #expect(throws: VersionError.invalid("1.02.3")) { try SemanticVersion.parseRelease("1.02.3") }
        // Pas de zéro initial dans un identifiant numérique de pré-version ; permis s'il contient une lettre.
        #expect(throws: VersionError.invalid("1.0.0-01")) { try SemanticVersion.parseRelease("1.0.0-01") }
        #expect(try SemanticVersion.parseRelease("1.0.0-0.0a.00a-").prerelease == "0.0a.00a-")
        #expect(throws: VersionError.invalid("1.0.0-a..b")) { try SemanticVersion.parseRelease("1.0.0-a..b") }
        #expect(throws: VersionError.invalid("1.0.0-a.")) { try SemanticVersion.parseRelease("1.0.0-a.") }
        #expect(throws: VersionError.invalid("1.0.0-é")) { try SemanticVersion.parseRelease("1.0.0-é") }
        #expect(throws: VersionError.invalid("１.0.0")) { try SemanticVersion.parseRelease("１.0.0") }
    }

    @Test("Précédence des pré-versions (SemVer §11)")
    func prereleasePrecedence() throws {
        func version(_ text: String) throws -> SemanticVersion {
            try SemanticVersion.parseRelease(text)
        }
        let beta = try version("1.4.0-beta.2")
        #expect(beta < (try version("1.4.0")))
        #expect(beta.prerelease == "beta.2")
        #expect((try version("1.4.0")).prerelease.isEmpty)

        #expect(try version("1.0.0-2") < version("1.0.0-10"))
        #expect(try version("1.0.0-9") < version("1.0.0-a"))
        #expect(try version("1.0.0-Z") < version("1.0.0-a"))
        #expect(try version("1.0.0-a.b") < version("1.0.0-a-b"))
        #expect(try version("1.0.0-a") < version("1.0.0-a.0"))
        #expect(try version("1.9.9") < version("1.10.0-alpha"))
        #expect(!(try version("1.0.0-rc.1") < version("1.0.0-rc.1")))
    }

    @Test("En JSON, une version se lit comme une publication et s'écrit sous sa forme canonique")
    func codable() throws {
        let version = try JSONDecoder().decode(SemanticVersion.self, from: Data("\"v2.1\"".utf8))
        #expect(version.description == "2.1.0")
        #expect(String(decoding: try JSONEncoder().encode(version), as: UTF8.self) == "\"2.1.0\"")
        #expect(throws: DecodingError.self) {
            try JSONDecoder().decode(SemanticVersion.self, from: Data("\"1.2.3+7\"".utf8))
        }
    }
}
