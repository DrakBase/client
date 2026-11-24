import AppsMarketCore
import AppsMarketCrypto
import AppsMarketTestSupport
import Foundation
import Testing

@Suite("Configuration lue dans l'Info.plist")
struct ConfigurationTests {
    static let feedURL = "https://maj.example.fr/api/v1/update/translate"

    let primary: String
    let backup: String

    init() throws {
        primary = try Fixtures.text("keys/test-app.pub").trimmingCharacters(in: .whitespacesAndNewlines)
        backup = try Fixtures.text("keys/other-app.pub").trimmingCharacters(in: .whitespacesAndNewlines)
    }

    func info(_ changes: [String: Any?] = [:]) -> [String: Any] {
        var info: [String: Any] = [
            UpdateFeed.infoPlistKey: Self.feedURL,
            UpdaterConfiguration.publicKeysKey: [primary, backup],
        ]
        for (key, value) in changes {
            info[key] = value
        }
        return info
    }

    @Test("Les quatre clés de docs/PROTOCOLE.md, la clé principale en premier")
    func allKeys() throws {
        let requirement = #"anchor apple generic and certificate leaf[subject.CN] = "Camille Essai""#
        let configuration = try UpdaterConfiguration(infoDictionary: info([
            UpdaterConfiguration.trustedRequirementsKey: [requirement],
            UpdaterConfiguration.installerServiceKey: "fr.example.pli.Installer",
        ]))
        #expect(UpdateFeed.infoPlistKey == "DrakBaseFeedURL")
        #expect(UpdaterConfiguration.publicKeysKey == "DrakBasePublicKeys")
        #expect(UpdaterConfiguration.trustedRequirementsKey == "DrakBaseTrustedRequirements")
        #expect(UpdaterConfiguration.installerServiceKey == "DrakBaseInstallerService")
        #expect(configuration.feed.baseURL.absoluteString == Self.feedURL)
        #expect(configuration.publicKeys.primary == (try MinisignPublicKey(tauri: primary)))
        #expect(configuration.publicKeys.backups == [try MinisignPublicKey(tauri: backup)])
        #expect(configuration.publicKeys.keys.count == 2)
        #expect(configuration.trustedRequirements == [requirement])
        #expect(configuration.installerService == "fr.example.pli.Installer")
    }

    @Test("Exigences et service facultatifs, une seule clé possible")
    func optionalKeys() throws {
        let configuration = try UpdaterConfiguration(infoDictionary: info([UpdaterConfiguration.publicKeysKey: [primary]]))
        #expect(configuration.publicKeys.backups.isEmpty)
        #expect(configuration.trustedRequirements.isEmpty)
        #expect(configuration.installerService == nil)
    }

    @Test("Chaque manque ou valeur illisible a son erreur")
    func errors() throws {
        #expect(throws: ConfigurationError.feed(.missingFeedURL)) {
            try UpdaterConfiguration(infoDictionary: info([UpdateFeed.infoPlistKey: nil]))
        }
        #expect(throws: ConfigurationError.missingPublicKeys) {
            try UpdaterConfiguration(infoDictionary: info([UpdaterConfiguration.publicKeysKey: nil]))
        }
        for invalid: Any in [[String](), primary, [42]] {
            #expect(throws: ConfigurationError.invalidPublicKeys) {
                try UpdaterConfiguration(infoDictionary: info([UpdaterConfiguration.publicKeysKey: invalid]))
            }
        }
        #expect(throws: ConfigurationError.invalidPublicKey(index: 1, .base64)) {
            try UpdaterConfiguration(infoDictionary: info([UpdaterConfiguration.publicKeysKey: [primary, "pas une clé"]]))
        }
        let key = try MinisignPublicKey(tauri: primary)
        #expect(throws: ConfigurationError.duplicatePublicKey(key.keyID)) {
            try UpdaterConfiguration(infoDictionary: info([UpdaterConfiguration.publicKeysKey: [primary, primary]]))
        }
        #expect(throws: ConfigurationError.invalidTrustedRequirements) {
            try UpdaterConfiguration(infoDictionary: info([UpdaterConfiguration.trustedRequirementsKey: "anchor apple"]))
        }
        #expect(throws: ConfigurationError.invalidTrustedRequirement("pas une exigence (")) {
            try UpdaterConfiguration(infoDictionary: info([UpdaterConfiguration.trustedRequirementsKey: ["pas une exigence ("]]))
        }
        for invalid: Any in ["", 42] {
            #expect(throws: ConfigurationError.invalidInstallerService) {
                try UpdaterConfiguration(infoDictionary: info([UpdaterConfiguration.installerServiceKey: invalid]))
            }
        }
        #expect(ConfigurationError.missingPublicKeys.localizedDescription == "DrakBasePublicKeys absent de l'Info.plist")
    }

    @Test("Les anciennes clés AppsMarket… sont lues quand les nouvelles manquent, jamais à leur place")
    func legacyKeys() throws {
        let requirement = #"anchor apple generic and certificate leaf[subject.CN] = "Camille Essai""#
        let legacy: [String: Any] = [
            "AppsMarketFeedURL": Self.feedURL,
            "AppsMarketPublicKeys": [primary, backup],
            "AppsMarketTrustedRequirements": [requirement],
            "AppsMarketInstallerService": "fr.example.pli.Installer",
        ]
        let configuration = try UpdaterConfiguration(infoDictionary: legacy)
        #expect(configuration.feed.baseURL.absoluteString == Self.feedURL)
        #expect(configuration.publicKeys.primary == (try MinisignPublicKey(tauri: primary)))
        #expect(configuration.publicKeys.backups == [try MinisignPublicKey(tauri: backup)])
        #expect(configuration.trustedRequirements == [requirement])
        #expect(configuration.installerService == "fr.example.pli.Installer")

        // Une nouvelle clé l'emporte sur l'ancienne, même illisible : une erreur n'est pas masquée.
        var both = legacy
        both[UpdaterConfiguration.publicKeysKey] = [backup]
        #expect(try UpdaterConfiguration(infoDictionary: both).publicKeys.primary == (try MinisignPublicKey(tauri: backup)))
        both[UpdaterConfiguration.publicKeysKey] = "pas un tableau"
        #expect(throws: ConfigurationError.invalidPublicKeys) { try UpdaterConfiguration(infoDictionary: both) }
        #expect(UpdaterConfiguration.legacyKey(for: UpdaterConfiguration.publicKeysKey) == "AppsMarketPublicKeys")
    }

    @Test("Configuration et app installée lues dans le bundle d'une app")
    func bundle() throws {
        let directory = try FakeApps.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let url = try FakeApps.make(in: directory, version: "1.2", info: info())
        let bundle = try #require(Bundle(url: url))

        let configuration = try UpdaterConfiguration(bundle: bundle)
        #expect(configuration.feed.baseURL.absoluteString == Self.feedURL)
        #expect(configuration.publicKeys.keys.count == 2)
        let app = try InstalledApp(bundle: bundle)
        #expect(app.bundleIdentifier == "fr.example.essai")
        #expect(app.version.description == "1.2.0")
        #expect(app.url.standardizedFileURL == url.standardizedFileURL)
    }

    @Test("L'app installée exige un identifiant et une version lisible")
    func installedAppErrors() throws {
        let directory = try FakeApps.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let unversioned = try FakeApps.make(in: directory, name: "SansVersion", version: "1.0")
        var info = try FakeApps.info(of: unversioned)
        info["CFBundleShortVersionString"] = nil
        try FakeApps.writeInfo(info, to: unversioned)
        #expect(throws: ConfigurationError.missingBundleVersion) { try InstalledApp(bundle: #require(Bundle(url: unversioned))) }

        let unreadable = try FakeApps.make(in: directory, name: "Illisible", version: "dernière")
        #expect(throws: ConfigurationError.invalidBundleVersion("dernière")) { try InstalledApp(bundle: #require(Bundle(url: unreadable))) }

        let anonymous = try FakeApps.make(in: directory, name: "Anonyme", version: "1.0")
        info = try FakeApps.info(of: anonymous)
        info["CFBundleIdentifier"] = nil
        try FakeApps.writeInfo(info, to: anonymous)
        #expect(throws: ConfigurationError.missingBundleIdentifier) { try InstalledApp(bundle: #require(Bundle(url: anonymous))) }
    }
}
