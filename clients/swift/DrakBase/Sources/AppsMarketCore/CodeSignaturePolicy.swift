import Foundation
import Security

/// Contrôle de la signature de code, entre l'app installée et le nouveau bundle.
///
/// Le code livré n'en a qu'une, `SecurityCodeSignaturePolicy`. Ce protocole n'est pas public :
/// seuls les tests du paquet en injectent une autre (`@testable import`), parce que leurs fausses
/// apps ne sont signées par aucun certificat.
protocol CodeSignaturePolicy: Sendable {
    /// Refuse une app installée non signée ou signée ad hoc, par `.unavailable` : son exigence
    /// désignée ne vaut que pour son binaire exact, aucune mise à jour ne pourrait la satisfaire.
    func checkInstalledApp(at url: URL) throws(InstallationError)

    /// Exigence désignée de l'app installée, en texte. Elle est lue avant l'échange : après, l'ancien
    /// bundle est dans le dossier de travail, que d'autres processus peuvent modifier.
    func designatedRequirement(ofInstalledApp url: URL) throws(InstallationError) -> String

    /// Le bundle doit être signé avec un certificat et satisfaire `designatedRequirement`, ou l'une
    /// des exigences de confiance (`DrakBaseTrustedRequirements`).
    func checkCandidate(at url: URL, designatedRequirement: String, trustedRequirements: [String]) throws(InstallationError)
}

/// Contrôle par Security.framework (`SecStaticCodeCheckValidityWithErrors`).
struct SecurityCodeSignaturePolicy: CodeSignaturePolicy {
    /// Toutes les architectures, le code imbriqué (frameworks, services XPC) et la validation
    /// stricte, comme `codesign --verify --strict --deep`.
    static let validityFlags = SecCSFlags(
        rawValue: kSecCSCheckAllArchitectures | kSecCSCheckNestedCode | kSecCSStrictValidate
    )

    enum SigningKind: Equatable {
        case unsigned
        case adHoc
        /// Signé avec un certificat, auto-signé compris.
        case certificate
    }

    func checkInstalledApp(at url: URL) throws(InstallationError) {
        switch Self.signingKind(of: try Self.staticCode(at: url)) {
        case .certificate:
            return
        case .adHoc:
            throw .unavailable(.adHocSignature)
        case .unsigned:
            throw .unavailable(.unsigned)
        }
    }

    func designatedRequirement(ofInstalledApp url: URL) throws(InstallationError) -> String {
        let requirement = try Self.designatedRequirement(of: Self.staticCode(at: url))
        var text: CFString?
        let status = SecRequirementCopyString(requirement, [], &text)
        guard status == errSecSuccess, let text else {
            throw .codeSignature("exigence désignée de l'app installée illisible : \(Self.message(for: status))")
        }
        return text as String
    }

    func checkCandidate(at url: URL, designatedRequirement: String, trustedRequirements: [String]) throws(InstallationError) {
        let candidate = try Self.staticCode(at: url)
        // Une exigence de confiance trop large ne suffit jamais à faire passer un bundle ad hoc.
        guard Self.signingKind(of: candidate) == .certificate else {
            throw .codeSignature("le nouveau bundle n'est pas signé avec un certificat")
        }
        let designated = try Self.requirement(designatedRequirement)
        let designatedFailure: String
        do {
            try Self.checkValidity(of: candidate, against: designated)
            return
        } catch {
            designatedFailure = error.detail
        }
        for text in trustedRequirements {
            let requirement = try Self.requirement(text)
            do {
                try Self.checkValidity(of: candidate, against: requirement)
                return
            } catch {
                continue
            }
        }
        throw .codeSignature(
            "le nouveau bundle ne satisfait ni l'exigence désignée de l'app installée, ni DrakBaseTrustedRequirements (\(designatedFailure))"
        )
    }

    // MARK: - Security.framework

    static func staticCode(at url: URL) throws(InstallationError) -> SecStaticCode {
        var code: SecStaticCode?
        let status = SecStaticCodeCreateWithPath(url as CFURL, [], &code)
        guard status == errSecSuccess, let code else {
            throw .codeSignature("lecture de la signature de \(url.lastPathComponent) impossible : \(message(for: status))")
        }
        return code
    }

    static func signingKind(of code: SecStaticCode) -> SigningKind {
        var information: CFDictionary?
        let status = SecCodeCopySigningInformation(code, SecCSFlags(rawValue: kSecCSSigningInformation), &information)
        guard status == errSecSuccess, let information = information as? [String: Any],
              information[kSecCodeInfoIdentifier as String] != nil
        else {
            return .unsigned
        }
        let flags = (information[kSecCodeInfoFlags as String] as? NSNumber)?.uint32Value ?? 0
        let certificates = information[kSecCodeInfoCertificates as String] as? [Any] ?? []
        if SecCodeSignatureFlags(rawValue: flags).contains(.adhoc) || certificates.isEmpty {
            return .adHoc
        }
        return .certificate
    }

    static func designatedRequirement(of code: SecStaticCode) throws(InstallationError) -> SecRequirement {
        var requirement: SecRequirement?
        let status = SecCodeCopyDesignatedRequirement(code, [], &requirement)
        guard status == errSecSuccess, let requirement else {
            throw .codeSignature("exigence désignée de l'app installée illisible : \(message(for: status))")
        }
        return requirement
    }

    static func requirement(_ text: String) throws(InstallationError) -> SecRequirement {
        var requirement: SecRequirement?
        let status = SecRequirementCreateWithString(text as CFString, [], &requirement)
        guard status == errSecSuccess, let requirement else {
            throw .codeSignature("exigence « \(text) » illisible : \(message(for: status))")
        }
        return requirement
    }

    /// Valide la signature du bundle entier, ressources comprises, contre une exigence.
    static func checkValidity(of code: SecStaticCode, against requirement: SecRequirement?) throws(InstallationError) {
        let status = SecStaticCodeCheckValidityWithErrors(code, validityFlags, requirement, nil)
        guard status == errSecSuccess else {
            throw .codeSignature(message(for: status))
        }
    }

    /// Texte de Security.framework pour un code d'erreur : « a sealed resource is missing or invalid ».
    static func message(for status: OSStatus) -> String {
        let text = SecCopyErrorMessageString(status, nil) as String?
        return "\(text ?? "erreur") (\(status))"
    }
}
