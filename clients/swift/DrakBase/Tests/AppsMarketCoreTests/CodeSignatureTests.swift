@testable import AppsMarketCore
import AppsMarketTestSupport
import Foundation
import Testing

/// Le vrai contrôle de signature de code, sans aucun certificat sur ce Mac : des fausses apps non
/// signées ou signées ad hoc, et des apps d'Apple, signées avec un certificat.
@Suite("Signature de code, par Security.framework")
struct CodeSignatureTests {
    static let calculator = URL(filePath: "/System/Applications/Calculator.app", directoryHint: .isDirectory)
    static let textEdit = URL(filePath: "/System/Applications/TextEdit.app", directoryHint: .isDirectory)

    let policy = SecurityCodeSignaturePolicy()

    @Test("App installée non signée ou signée ad hoc : indisponible")
    func installedApp() throws {
        let directory = try FakeApps.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let unsigned = try FakeApps.make(in: directory, name: "NonSignee", version: "1.0.0")
        #expect(throws: InstallationError.unavailable(.unsigned)) { try policy.checkInstalledApp(at: unsigned) }

        let adHoc = try FakeApps.make(in: directory, name: "AdHoc", version: "1.0.0")
        try FakeApps.signAdHoc(adHoc)
        #expect(try SecurityCodeSignaturePolicy.signingKind(of: SecurityCodeSignaturePolicy.staticCode(at: adHoc)) == .adHoc)
        #expect(throws: InstallationError.unavailable(.adHocSignature)) { try policy.checkInstalledApp(at: adHoc) }

        #expect(throws: Never.self) { try policy.checkInstalledApp(at: Self.calculator) }
        #expect(try SecurityCodeSignaturePolicy.signingKind(of: SecurityCodeSignaturePolicy.staticCode(at: Self.calculator)) == .certificate)
    }

    @Test("Nouveau bundle contrôlé contre l'exigence désignée de l'app installée")
    func designatedRequirement() throws {
        let requirement = try policy.designatedRequirement(ofInstalledApp: Self.calculator)
        #expect(requirement == #"identifier "com.apple.calculator" and anchor apple"#)
        #expect(throws: Never.self) {
            try policy.checkCandidate(at: Self.calculator, designatedRequirement: requirement, trustedRequirements: [])
        }
        let error = #expect(throws: InstallationError.self) {
            try policy.checkCandidate(at: Self.textEdit, designatedRequirement: requirement, trustedRequirements: [])
        }
        guard case .codeSignature(let detail) = error else {
            Issue.record("erreur inattendue : \(String(describing: error))")
            return
        }
        // Le message nomme la clé de l'Info.plist telle qu'on l'écrit aujourd'hui, pas son ancien nom.
        #expect(detail.hasPrefix("le nouveau bundle ne satisfait ni l'exigence désignée de l'app installée, ni DrakBaseTrustedRequirements ("))
        #expect(!detail.contains("AppsMarket"))
    }

    @Test("Ou contre une exigence de confiance, mais jamais pour un bundle ad hoc")
    func trustedRequirements() throws {
        let requirement = try policy.designatedRequirement(ofInstalledApp: Self.calculator)
        #expect(throws: Never.self) {
            try policy.checkCandidate(
                at: Self.textEdit, designatedRequirement: requirement,
                trustedRequirements: [#"identifier "fr.example.autre""#, #"identifier "com.apple.TextEdit" and anchor apple"#]
            )
        }

        let directory = try FakeApps.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let adHoc = try FakeApps.make(in: directory, bundleIdentifier: "com.apple.calculator", version: "99.0")
        try FakeApps.signAdHoc(adHoc)
        // « identifier … » seul serait satisfait par une signature ad hoc : le bundle est refusé avant.
        #expect(throws: InstallationError.codeSignature("le nouveau bundle n'est pas signé avec un certificat")) {
            try policy.checkCandidate(at: adHoc, designatedRequirement: requirement, trustedRequirements: [#"identifier "com.apple.calculator""#])
        }
        #expect(throws: InstallationError.self) {
            try policy.checkCandidate(at: Self.textEdit, designatedRequirement: requirement, trustedRequirements: ["pas une exigence ("])
        }
        // L'exigence désignée d'une app ad hoc ne vaut que pour son binaire exact.
        #expect(try policy.designatedRequirement(ofInstalledApp: adHoc).hasPrefix("cdhash H\""))
    }

    @Test("Un bundle modifié après sa signature ne passe plus la validation")
    func modifiedBundle() throws {
        let directory = try FakeApps.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let app = try FakeApps.make(in: directory, version: "1.0.0")
        try FakeApps.signAdHoc(app)
        let code = try SecurityCodeSignaturePolicy.staticCode(at: app)
        let designated = try SecurityCodeSignaturePolicy.designatedRequirement(of: code)
        #expect(throws: Never.self) { try SecurityCodeSignaturePolicy.checkValidity(of: code, against: designated) }

        try Data("echo modifié\n".utf8).write(to: app.appending(path: "Contents/MacOS/Essai"))
        #expect(throws: InstallationError.self) {
            try SecurityCodeSignaturePolicy.checkValidity(of: SecurityCodeSignaturePolicy.staticCode(at: app), against: designated)
        }
    }
}
