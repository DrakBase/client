@testable import AppsMarketCore
import AppsMarketTestSupport
import Foundation
import Testing

@Suite("Versions d'OS")
struct SystemVersionTests {
    func version(_ text: String) throws -> SystemVersion {
        try #require(SystemVersion(text), "« \(text) » devrait être lisible")
    }

    @Test("Lecture de fixtures/os-versions.json : les nombres lus, ou un texte illisible")
    func fixtureParsing() throws {
        let fixtures = try Fixtures.osVersions()
        #expect(fixtures.parse.contains { $0.components == nil })
        for testCase in fixtures.parse {
            #expect(SystemVersion(testCase.input)?.components == testCase.components, "« \(testCase.input) »")
        }
    }

    @Test("Ordre et égalités de fixtures/os-versions.json")
    func fixtureOrdering() throws {
        let fixtures = try Fixtures.osVersions()
        let ordered = try fixtures.ordered.map(version)
        #expect(ordered.count > 1)
        for (index, lower) in ordered.enumerated() {
            for higher in ordered[(index + 1)...] {
                #expect(lower < higher, "\(lower) devrait précéder \(higher)")
                #expect(!(higher < lower), "\(higher) ne devrait pas précéder \(lower)")
                #expect(lower != higher)
            }
        }
        #expect(!fixtures.equal.isEmpty)
        for pair in fixtures.equal {
            #expect(pair.count == 2)
            let first = try version(pair[0])
            let second = try version(pair[1])
            #expect(first == second, "« \(pair[0]) » et « \(pair[1]) »")
            #expect(!(first < second) && !(second < first), "« \(pair[0]) » et « \(pair[1]) »")
        }
    }

    @Test("Comparaisons à un minimum de fixtures/os-versions.json")
    func fixtureMinimums() throws {
        let fixtures = try Fixtures.osVersions()
        #expect(Set(fixtures.atLeast.map(\.expected)) == [true, false])
        for testCase in fixtures.atLeast {
            let isAtLeast = try version(testCase.minimum) <= version(testCase.version)
            #expect(isAtLeast == testCase.expected, "« \(testCase.version) » au moins « \(testCase.minimum) »")
        }
    }

    @Test("La version de macOS du processus garde ses trois nombres, pour les messages")
    func operatingSystemVersion() throws {
        let sonoma = SystemVersion(OperatingSystemVersion(majorVersion: 14, minorVersion: 6, patchVersion: 0))
        #expect(sonoma.description == "14.6.0")
        #expect(sonoma == (try version("14.6")))
    }
}
