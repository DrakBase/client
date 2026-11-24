import AppsMarketCrypto
import Foundation

/// Version d'un système, comme `LSMinimumSystemVersion` ou `min_os_version` : des nombres séparés
/// par des points (`docs/PROTOCOLE.md`, « Versions d'OS »). Les versions se comparent composant par
/// composant, un composant absent valant 0 : `14.6.1` > `14.6` > `14.0`, et `14` égale `14.0.0`.
/// Mêmes règles que `am_protocol::OsVersion`, sur les cas de `fixtures/os-versions.json`.
struct SystemVersion: Comparable, Sendable, CustomStringConvertible {
    let components: [UInt64]

    /// Lecture souple, celle du serveur : les blancs autour sont retirés, puis les nombres de tête
    /// comptent. Elle s'arrête au premier composant qui n'est pas fait que de chiffres, dont elle
    /// garde les chiffres de tête : `6.8.0-45-generic` vaut `6.8.0`, et `2..1` vaut `2`. Les zéros
    /// finaux ne sont pas gardés.
    ///
    /// `nil` pour un texte illisible : celui qui ne commence pas par un chiffre (`v14`, `.5`, un
    /// texte vide), qui dépasse 64 octets, ou dont un nombre ne tient pas sur 64 bits.
    init?(_ text: String) {
        let trimmed = text.trimmingUnicodeWhitespace().utf8
        guard trimmed.count <= 64 else { return nil }
        var components: [UInt64] = []
        for part in trimmed.split(separator: UInt8(ascii: "."), omittingEmptySubsequences: false) {
            let digits = part.prefix { (UInt8(ascii: "0")...UInt8(ascii: "9")).contains($0) }
            guard !digits.isEmpty else { break }
            guard let value = UInt64(String(decoding: digits, as: UTF8.self)) else { return nil }
            components.append(value)
            guard digits.count == part.count else { break }
        }
        while components.count > 1, components.last == 0 {
            components.removeLast()
        }
        guard !components.isEmpty else { return nil }
        self.components = components
    }

    init(_ version: OperatingSystemVersion) {
        components = [version.majorVersion, version.minorVersion, version.patchVersion].map { UInt64(clamping: $0) }
    }

    var description: String {
        components.map(String.init).joined(separator: ".")
    }

    static func < (lhs: SystemVersion, rhs: SystemVersion) -> Bool {
        compare(lhs, rhs) < 0
    }

    static func == (lhs: SystemVersion, rhs: SystemVersion) -> Bool {
        compare(lhs, rhs) == 0
    }

    private static func compare(_ lhs: SystemVersion, _ rhs: SystemVersion) -> Int {
        for index in 0..<max(lhs.components.count, rhs.components.count) {
            let left = index < lhs.components.count ? lhs.components[index] : 0
            let right = index < rhs.components.count ? rhs.components[index] : 0
            if left != right { return left < right ? -1 : 1 }
        }
        return 0
    }
}
