import AppsMarketCrypto
import CryptoKit
import Foundation

/// Clé minisign jetable, tirée par chaque test : elle signe comme `drakbase sign` et Tauri
/// (algorithme « ED », commentaire de confiance avec `version:`), sans fichier ni mot de passe.
public struct TestSigner: Sendable {
    public let keyID: [UInt8]
    private let privateKey: Curve25519.Signing.PrivateKey

    public init() {
        keyID = (0..<8).map { _ in UInt8.random(in: 0...255) }
        privateKey = Curve25519.Signing.PrivateKey()
    }

    /// Clé publique au format Tauri, comme une entrée d'`DrakBasePublicKeys`.
    public var publicKeyTauri: String {
        let data = Array("Ed".utf8) + keyID + Array(privateKey.publicKey.rawRepresentation)
        let text = "untrusted comment: minisign public key: \(Data(keyID.reversed()).hexString.uppercased())\n"
            + Data(data).base64EncodedString() + "\n"
        return Data(text.utf8).base64EncodedString()
    }

    public var publicKey: MinisignPublicKey {
        get throws {
            try MinisignPublicKey(tauri: publicKeyTauri)
        }
    }

    /// Signature au format Tauri d'un contenu. Sans `version`, le commentaire de confiance n'a pas
    /// ce champ, comme une signature faite sans `--app-version`.
    public func signature(for data: Data, version: String?, fileName: String = "Essai.app.tar.gz") throws -> String {
        let fileSignature = try Array(privateKey.signature(for: Data(BLAKE2b512.hash(data: data))))
        var comment = "timestamp:\(Int(Date().timeIntervalSince1970))\tfile:\(fileName)"
        if let version {
            comment += "\tversion:\(version)"
        }
        let globalSignature = try privateKey.signature(for: Data(fileSignature + Array(comment.utf8)))
        let text = "untrusted comment: signature from tauri secret key\n"
            + Data(Array("ED".utf8) + keyID + fileSignature).base64EncodedString() + "\n"
            + "trusted comment: \(comment)\n"
            + globalSignature.base64EncodedString() + "\n"
        return Data(text.utf8).base64EncodedString()
    }

    public func signature(forFileAt url: URL, version: String?) throws -> String {
        try signature(for: Data(contentsOf: url), version: version, fileName: url.lastPathComponent)
    }
}
