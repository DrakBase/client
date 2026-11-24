import Foundation

/// Jeux d'essai communs à Rust et Swift, lus en place dans le dossier `fixtures/` du dépôt
/// (voir `fixtures/README.md`). Chaque implémentation doit y donner exactement les résultats attendus.
public enum Fixtures {
    /// Dossier `fixtures/`, retrouvé à partir de ce fichier :
    /// `clients/swift/DrakBase/Tests/AppsMarketTestSupport/Fixtures.swift`.
    public static let directory: URL = URL(filePath: #filePath)
        .deletingLastPathComponent() // AppsMarketTestSupport
        .deletingLastPathComponent() // Tests
        .deletingLastPathComponent() // DrakBase
        .deletingLastPathComponent() // swift
        .deletingLastPathComponent() // clients
        .deletingLastPathComponent() // racine du dépôt
        .appending(path: "fixtures", directoryHint: .isDirectory)

    /// Fichier désigné comme dans les JSON des jeux d'essai : `keys/test-app.pub`.
    public static func url(_ relativePath: String) -> URL {
        directory.appending(path: relativePath, directoryHint: .notDirectory)
    }

    public static func data(_ relativePath: String) throws -> Data {
        try Data(contentsOf: url(relativePath))
    }

    public static func text(_ relativePath: String) throws -> String {
        try String(contentsOf: url(relativePath), encoding: .utf8)
    }

    public static func versions() throws -> VersionFixtures {
        try decode("versions.json")
    }

    public static func blake2b() throws -> BLAKE2bFixtures {
        try decode("blake2b.json")
    }

    public static func signatures() throws -> SignatureFixtures {
        try decode("signatures.json")
    }

    public static func osVersions() throws -> OSVersionFixtures {
        try decode("os-versions.json")
    }

    /// Les cas de `fixtures/update-response.json`. Les corps y sont du JSON quelconque : ils sont
    /// lus sans type, puis remis en JSON tels quels.
    public static func updateResponses() throws -> [UpdateResponseFixture] {
        guard let root = try JSONSerialization.jsonObject(with: data("update-response.json")) as? [String: Any],
              let cases = root["cases"] as? [[String: Any]]
        else {
            throw FixtureError.malformed("update-response.json")
        }
        return try cases.map(UpdateResponseFixture.init(json:))
    }

    /// Corps complet d'une réponse 200 : celui du premier cas accepté qui porte les neuf champs. Les
    /// tests qui fabriquent une réponse partent de lui, et n'en changent que les valeurs.
    public static func completeUpdateResponse() throws -> [String: Any] {
        let complete = try updateResponses().first { fixture in
            fixture.isAccepted && UpdateResponseFixture.fields.allSatisfy { fixture.fields?[$0] is String || fixture.fields?[$0] is NSNumber }
        }
        guard let fields = complete?.fields else { throw FixtureError.malformed("update-response.json") }
        return fields
    }

    /// Octets déterministes des jeux d'essai : l'octet `i` vaut `i % 251`.
    public static func pattern(length: Int) -> [UInt8] {
        (0..<length).map { UInt8($0 % 251) }
    }

    private static func decode<T: Decodable>(_ name: String) throws -> T {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        return try decoder.decode(T.self, from: data(name))
    }
}

/// `fixtures/versions.json`.
public struct VersionFixtures: Decodable, Sendable {
    public struct Normalization: Decodable, Sendable {
        public let input: String
        /// Forme canonique attendue pour une version de publication, `nil` si elle est refusée.
        public let release: String?
        /// Forme canonique attendue pour une version envoyée par une app, `nil` si elle est refusée.
        public let client: String?
    }

    public let normalize: [Normalization]
    /// Versions rangées de la plus basse à la plus haute.
    public let ordered: [String]
}

/// `fixtures/os-versions.json`.
public struct OSVersionFixtures: Decodable, Sendable {
    public struct Parsing: Decodable, Sendable {
        public let input: String
        /// Nombres lus, sans les zéros finaux ; `nil` si le texte est illisible.
        public let components: [UInt64]?
    }

    public struct Minimum: Decodable, Sendable {
        public let version: String
        public let minimum: String
        /// `version` vaut-elle au moins `minimum` ?
        public let expected: Bool
    }

    public let parse: [Parsing]
    /// Versions strictement croissantes.
    public let ordered: [String]
    /// Paires de textes qui désignent la même version.
    public let equal: [[String]]
    public let atLeast: [Minimum]
}

/// Un cas de `fixtures/update-response.json` : un corps de réponse 200, et ce qu'un client en fait.
public struct UpdateResponseFixture {
    /// Les neuf champs d'une réponse.
    public static let fields = ["version", "url", "signature", "notes", "pub_date", "size", "sha256", "min_os_version", "channel"]

    public let name: String
    /// Le corps, en JSON, tel que le serveur l'enverrait.
    public let body: Data
    /// Le même corps, s'il est un objet.
    public let fields: [String: Any]?
    /// Vrai si le client l'accepte, faux s'il le refuse.
    public let isAccepted: Bool
    /// Pour un cas accepté, la valeur lue de chaque champ ; `NSNull` pour `notes` et
    /// `min_os_version` absents.
    public let check: [String: Any]

    init(json: [String: Any]) throws {
        guard let name = json["name"] as? String, let body = json["body"], let expected = json["expected"] as? String,
              ["ok", "refused"].contains(expected)
        else {
            throw FixtureError.malformed("update-response.json")
        }
        self.name = name
        self.body = try JSONSerialization.data(withJSONObject: body, options: [.fragmentsAllowed])
        fields = body as? [String: Any]
        isAccepted = expected == "ok"
        guard let check = json["check"] as? [String: Any] ?? (isAccepted ? nil : [:]) else {
            throw FixtureError.malformed("update-response.json, cas « \(name) »")
        }
        self.check = check
    }
}

public enum FixtureError: Error {
    /// Le jeu d'essai n'a pas la forme décrite dans `fixtures/README.md`.
    case malformed(String)
}

/// `fixtures/blake2b.json`.
public struct BLAKE2bFixtures: Decodable, Sendable {
    public struct Vector: Decodable, Sendable {
        /// Empreinte attendue, en hexadécimal.
        public let digest: String
        public let text: String?
        public let patternLength: Int?

        /// Données hachées : le texte en UTF-8, sinon le motif de `patternLength` octets.
        public var input: [UInt8] {
            text.map { Array($0.utf8) } ?? Fixtures.pattern(length: patternLength ?? 0)
        }

        public var label: String {
            text.map { "texte « \($0) »" } ?? "motif de \(patternLength ?? 0) octets"
        }
    }

    public let algorithm: String
    public let vectors: [Vector]
}

/// `fixtures/signatures.json`.
public struct SignatureFixtures: Decodable, Sendable {
    public struct Case: Decodable, Sendable {
        public let name: String
        public let key: String
        public let file: String
        public let signature: String
        public let announcedVersion: String
        /// Un des `expectedCodes`.
        public let expected: String
    }

    /// Lecture d'une clé ou d'une signature, sans vérification.
    public struct Parsing: Decodable, Sendable {
        public let name: String
        /// `public_key` ou `signature`.
        public let kind: String
        public let file: String
        /// `ok` ou `malformed`.
        public let expected: String
        /// Identifiant de clé attendu quand la lecture réussit, comme l'affiche minisign.
        public let keyId: String?
    }

    public let cases: [Case]
    public let expectedCodes: [String]
    public let parsing: [Parsing]
    public let parsingCodes: [String]
}

extension Data {
    /// Hexadécimal minuscule, comme dans les jeux d'essai.
    public var hexString: String {
        map { String(format: "%02x", $0) }.joined()
    }
}

extension Array where Element == UInt8 {
    /// Hexadécimal minuscule, comme dans les jeux d'essai.
    public var hexString: String {
        Data(self).hexString
    }
}
