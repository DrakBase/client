import AppsMarketCrypto
import Foundation

/// Version canonique : trois composants numériques, une pré-version éventuelle, jamais de
/// métadonnées de build. Lecture souple à l'entrée (`v1.2` devient `1.2.0`), mêmes règles et même
/// ordre que `am_protocol::version` (`fixtures/versions.json`).
public struct SemanticVersion: Hashable, Sendable, Comparable, CustomStringConvertible {
    public let major: UInt64
    public let minor: UInt64
    public let patch: UInt64
    /// Pré-version, vide pour une version finale : `beta.2` pour `1.4.0-beta.2`.
    public let prerelease: String

    /// Forme canonique, la seule que le serveur annonce et que l'outil signe : `1.4.0-beta.2`.
    public var description: String {
        let core = "\(major).\(minor).\(patch)"
        return prerelease.isEmpty ? core : "\(core)-\(prerelease)"
    }

    /// Lit la version d'une publication. Accepte `v1.2` ou `1.2`, refuse `+build`.
    public static func parseRelease(_ input: String) throws(VersionError) -> SemanticVersion {
        let (version, hasBuildMetadata) = try parseLenient(input)
        guard !hasBuildMetadata else {
            throw .buildMetadata(input.trimmingUnicodeWhitespace())
        }
        return version
    }

    /// Lit la version d'une app cliente (`CFBundleShortVersionString`). Mêmes règles, mais
    /// `+build` est ignoré.
    public static func parseClient(_ input: String) throws(VersionError) -> SemanticVersion {
        try parseLenient(input).version
    }

    /// Précédence SemVer 2.0.0 (§11). Comme il n'y a jamais de métadonnées de build, cet ordre reste
    /// cohérent avec l'égalité.
    public static func < (lhs: SemanticVersion, rhs: SemanticVersion) -> Bool {
        if lhs.major != rhs.major { return lhs.major < rhs.major }
        if lhs.minor != rhs.minor { return lhs.minor < rhs.minor }
        if lhs.patch != rhs.patch { return lhs.patch < rhs.patch }
        return comparePrereleases(lhs.prerelease, rhs.prerelease) < 0
    }

    // MARK: - Lecture

    private static func parseLenient(_ input: String) throws(VersionError) -> (version: SemanticVersion, hasBuildMetadata: Bool) {
        let trimmed = input.trimmingUnicodeWhitespace()
        var body = Array(trimmed.utf8)[...]
        if body.first == UInt8(ascii: "v") || body.first == UInt8(ascii: "V") {
            body.removeFirst()
        }
        guard !body.isEmpty else { throw .empty }
        // Le cœur numérique s'arrête à la pré-version (-) ou aux métadonnées de build (+).
        let coreEnd = body.firstIndex { $0 == UInt8(ascii: "-") || $0 == UInt8(ascii: "+") } ?? body.endIndex
        let components = body[..<coreEnd].reduce(1) { $1 == UInt8(ascii: ".") ? $0 + 1 : $0 }
        let padding: String
        switch components {
        case 1: padding = ".0.0"
        case 2: padding = ".0"
        case 3: padding = ""
        default: throw .tooManyComponents(trimmed)
        }
        let padded = Array(body[..<coreEnd]) + Array(padding.utf8) + Array(body[coreEnd...])
        guard let parsed = parseStrict(padded[...]) else { throw .invalid(trimmed) }
        return parsed
    }

    /// Lecture stricte, comme `semver::Version::parse` : `MAJEUR.MINEUR.CORRECTIF[-pré-version][+build]`,
    /// sans zéro initial dans les nombres, ni dans les identifiants numériques de la pré-version.
    private static func parseStrict(_ text: ArraySlice<UInt8>) -> (version: SemanticVersion, hasBuildMetadata: Bool)? {
        var rest = text
        guard let major = numericIdentifier(&rest), rest.popFirst() == UInt8(ascii: "."),
              let minor = numericIdentifier(&rest), rest.popFirst() == UInt8(ascii: "."),
              let patch = numericIdentifier(&rest)
        else {
            return nil
        }
        var prerelease = ""
        if rest.first == UInt8(ascii: "-") {
            rest.removeFirst()
            guard let identifiers = dotSeparatedIdentifiers(&rest, isPrerelease: true) else { return nil }
            prerelease = String(decoding: identifiers, as: UTF8.self)
        }
        var hasBuildMetadata = false
        if rest.first == UInt8(ascii: "+") {
            rest.removeFirst()
            guard dotSeparatedIdentifiers(&rest, isPrerelease: false) != nil else { return nil }
            hasBuildMetadata = true
        }
        guard rest.isEmpty else { return nil }
        return (SemanticVersion(major: major, minor: minor, patch: patch, prerelease: prerelease), hasBuildMetadata)
    }

    /// Nombre en chiffres ASCII, sans zéro initial, qui tient sur 64 bits.
    private static func numericIdentifier(_ rest: inout ArraySlice<UInt8>) -> UInt64? {
        var value: UInt64 = 0
        var length = 0
        while let digit = rest.first, isDigit(digit) {
            if value == 0, length > 0 { return nil }
            let (product, productOverflow) = value.multipliedReportingOverflow(by: 10)
            let (sum, sumOverflow) = product.addingReportingOverflow(UInt64(digit - UInt8(ascii: "0")))
            if productOverflow || sumOverflow { return nil }
            value = sum
            length += 1
            rest.removeFirst()
        }
        return length > 0 ? value : nil
    }

    /// Identifiants `[0-9A-Za-z-]+` séparés par des points, lus jusqu'au premier autre caractère.
    /// Aucun ne peut être vide ; dans une pré-version, un identifiant numérique n'a pas de zéro
    /// initial.
    private static func dotSeparatedIdentifiers(_ rest: inout ArraySlice<UInt8>, isPrerelease: Bool) -> ArraySlice<UInt8>? {
        let text = rest
        var segmentStart = rest.startIndex
        var segmentIsNumeric = true
        while true {
            if let byte = rest.first, isDigit(byte) || isLetter(byte) || byte == UInt8(ascii: "-") {
                segmentIsNumeric = segmentIsNumeric && isDigit(byte)
                rest.removeFirst()
                continue
            }
            let segmentLength = rest.startIndex - segmentStart
            guard segmentLength > 0 else { return nil }
            if isPrerelease, segmentIsNumeric, segmentLength > 1, text[segmentStart] == UInt8(ascii: "0") {
                return nil
            }
            guard rest.first == UInt8(ascii: ".") else {
                return text[text.startIndex..<rest.startIndex]
            }
            rest.removeFirst()
            segmentStart = rest.startIndex
            segmentIsNumeric = true
        }
    }

    /// Ordre des pré-versions (SemVer §11.4) : une version finale passe après ses pré-versions ;
    /// identifiant par identifiant, les numériques se comparent en valeur et passent avant les
    /// autres, comparés en ASCII ; à égalité, la liste la plus longue l'emporte.
    private static func comparePrereleases(_ lhs: String, _ rhs: String) -> Int {
        if lhs.utf8.elementsEqual(rhs.utf8) { return 0 }
        if lhs.isEmpty { return 1 }
        if rhs.isEmpty { return -1 }
        var rhsIdentifiers = rhs.utf8.split(separator: UInt8(ascii: "."), omittingEmptySubsequences: false).makeIterator()
        for left in lhs.utf8.split(separator: UInt8(ascii: "."), omittingEmptySubsequences: false) {
            guard let right = rhsIdentifiers.next() else { return 1 }
            let order: Int
            switch (left.allSatisfy(isDigit), right.allSatisfy(isDigit)) {
            case (true, true):
                // Pas de zéro initial : le plus long est le plus grand.
                order = left.count != right.count ? (left.count < right.count ? -1 : 1) : compareBytes(left, right)
            case (true, false):
                return -1
            case (false, true):
                return 1
            case (false, false):
                order = compareBytes(left, right)
            }
            if order != 0 { return order }
        }
        return rhsIdentifiers.next() == nil ? 0 : -1
    }

    private static func compareBytes(_ lhs: Substring.UTF8View, _ rhs: Substring.UTF8View) -> Int {
        if lhs.elementsEqual(rhs) { return 0 }
        return lhs.lexicographicallyPrecedes(rhs) ? -1 : 1
    }

    private static func isDigit(_ byte: UInt8) -> Bool {
        (UInt8(ascii: "0")...UInt8(ascii: "9")).contains(byte)
    }

    private static func isLetter(_ byte: UInt8) -> Bool {
        (UInt8(ascii: "A")...UInt8(ascii: "Z")).contains(byte) || (UInt8(ascii: "a")...UInt8(ascii: "z")).contains(byte)
    }
}

/// Une version se code en JSON sous sa forme canonique, et se lit comme une version de publication.
extension SemanticVersion: Codable {
    public init(from decoder: any Decoder) throws {
        let container = try decoder.singleValueContainer()
        let raw = try container.decode(String.self)
        do {
            self = try Self.parseRelease(raw)
        } catch {
            throw DecodingError.dataCorruptedError(in: container, debugDescription: error.localizedDescription)
        }
    }

    public func encode(to encoder: any Encoder) throws {
        var container = encoder.singleValueContainer()
        try container.encode(description)
    }
}

/// Version illisible. Mêmes cas que `am_protocol::VersionError`.
public enum VersionError: Error, Equatable, Sendable, LocalizedError {
    case empty
    /// Quatre composants ou plus, comme `1.2.3.4`.
    case tooManyComponents(String)
    case invalid(String)
    /// Métadonnées de build (`+…`) dans une version de publication.
    case buildMetadata(String)

    public var errorDescription: String? {
        switch self {
        case .empty:
            "version vide"
        case .tooManyComponents(let version):
            "version « \(version) » : quatre composants ou plus"
        case .invalid(let version):
            "version « \(version) » illisible"
        case .buildMetadata(let version):
            "version « \(version) » : les métadonnées de build (+…) sont interdites"
        }
    }
}
