import AppsMarketCore
import Foundation
import Testing

@Suite("Adresse de vérification")
struct UpdateFeedTests {
    static let base = "https://maj.example.fr/api/v1/update/translate"

    let feed: UpdateFeed
    let version: SemanticVersion

    init() throws {
        feed = try UpdateFeed(baseURL: #require(URL(string: Self.base)))
        version = try SemanticVersion.parseClient("v1.2+42")
    }

    static func os(_ major: Int, _ minor: Int, _ patch: Int) -> OperatingSystemVersion {
        OperatingSystemVersion(majorVersion: major, minorVersion: minor, patchVersion: patch)
    }

    @Test("Cible darwin, arch, version canonique, puis bundle_type, channel et os_version")
    func checkURL() throws {
        let url = feed.checkURL(currentVersion: version, channel: .stable, architecture: .aarch64, osVersion: Self.os(14, 6, 0))
        #expect(url.absoluteString == "\(Self.base)/darwin/aarch64/1.2.0?bundle_type=app&channel=stable&os_version=14.6")

        let beta = feed.checkURL(
            currentVersion: try SemanticVersion.parseRelease("1.4.0-beta.2"),
            channel: .beta, architecture: .x86_64, osVersion: Self.os(26, 4, 1)
        )
        #expect(beta.absoluteString == "\(Self.base)/darwin/x86_64/1.4.0-beta.2?bundle_type=app&channel=beta&os_version=26.4.1")
    }

    @Test("Une barre finale, un port ou l'absence de chemin ne changent rien au reste")
    func baseURLVariants() throws {
        let expectations = [
            "\(Self.base)/": "\(Self.base)/darwin/aarch64/1.2.0",
            "http://localhost:8080/api/v1/update/demo": "http://localhost:8080/api/v1/update/demo/darwin/aarch64/1.2.0",
            "https://maj.example.fr": "https://maj.example.fr/darwin/aarch64/1.2.0",
        ]
        for (base, expected) in expectations {
            let feed = try UpdateFeed(baseURL: #require(URL(string: base)))
            let url = feed.checkURL(currentVersion: version, channel: .stable, architecture: .aarch64, osVersion: Self.os(15, 0, 0))
            #expect(url.absoluteString == "\(expected)?bundle_type=app&channel=stable&os_version=15.0", "base « \(base) »")
        }
    }

    @Test("Par défaut, l'arch et la version de macOS sont celles du processus")
    func defaults() throws {
        let url = feed.checkURL(currentVersion: version, channel: .beta)
        #if arch(arm64)
        #expect(CPUArchitecture.current == .aarch64)
        #else
        #expect(CPUArchitecture.current == .x86_64)
        #endif
        #expect(url.path() == "/api/v1/update/translate/darwin/\(CPUArchitecture.current.rawValue)/1.2.0")

        let system = ProcessInfo.processInfo.operatingSystemVersion
        let expected = feed.checkURL(currentVersion: version, channel: .beta, architecture: .current, osVersion: system)
        #expect(url == expected)
        let query = try #require(URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems)
        #expect(query.map(\.name) == ["bundle_type", "channel", "os_version"])
        #expect(query.last?.value?.hasPrefix("\(system.majorVersion).\(system.minorVersion)") == true)
    }

    @Test("DrakBaseFeedURL est lu dans l'Info.plist, sinon l'ancienne AppsMarketFeedURL")
    func infoPlist() throws {
        let feed = try UpdateFeed(infoDictionary: [UpdateFeed.infoPlistKey: Self.base])
        #expect(feed == self.feed)
        #expect(UpdateFeed.infoPlistKey == "DrakBaseFeedURL")
        #expect(try UpdateFeed(infoDictionary: ["AppsMarketFeedURL": Self.base]) == self.feed)
        #expect(throws: UpdateFeedError.invalidFeedURL("42")) {
            try UpdateFeed(infoDictionary: [UpdateFeed.infoPlistKey: 42, UpdateFeed.legacyInfoPlistKey: Self.base])
        }
        #expect(throws: UpdateFeedError.missingFeedURL) { try UpdateFeed(infoDictionary: [:]) }
        #expect(throws: UpdateFeedError.invalidFeedURL("42")) { try UpdateFeed(infoDictionary: [UpdateFeed.infoPlistKey: 42]) }
    }

    @Test("Seule une adresse http(s) absolue, sans requête ni fragment, est acceptée")
    func invalidBaseURLs() throws {
        for raw in [
            "ftp://maj.example.fr/api/v1/update/translate",
            "/api/v1/update/translate",
            "maj.example.fr/api/v1/update/translate",
            "https:///api/v1/update/translate",
            "\(Self.base)?canal=beta",
            "\(Self.base)#ici",
        ] {
            let url = try #require(URL(string: raw))
            #expect(throws: UpdateFeedError.invalidFeedURL(url.absoluteString), "« \(raw) »") {
                try UpdateFeed(baseURL: url)
            }
        }
        #expect(UpdateFeedError.missingFeedURL.localizedDescription == "DrakBaseFeedURL absent de l'Info.plist")
    }
}
