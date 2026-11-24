import Foundation
import OSLog

private let log = Logger(subsystem: "DrakBase", category: "check")

/// Une vérification auprès du serveur (`docs/PROTOCOLE.md`, « Vérification »).
struct UpdateCheck: Sendable {
    /// En-tête de l'identifiant d'installation, jamais dans l'adresse.
    static let installIDHeader = "X-AppsMarket-Install-Id"
    /// Au-delà, le corps de la réponse n'est pas lu : une description de mise à jour tient en
    /// quelques kilo-octets, notes comprises.
    static let maximumBodySize = 1 << 20
    /// Durée maximale d'une vérification, réponse comprise : un serveur trop lent ne bloque pas les
    /// opérations suivantes.
    static let defaultTimeout: Duration = .seconds(60)

    let feed: UpdateFeed
    let session: URLSession
    let currentVersion: SemanticVersion
    let channel: UpdateChannel
    let installID: UUID?
    let architecture: CPUArchitecture
    let systemVersion: OperatingSystemVersion
    var timeout: Duration = Self.defaultTimeout

    /// La mise à jour proposée, ou `nil` si l'app est à jour. Une version qui ne serait pas plus
    /// récente, ou qui exigerait un macOS plus récent, compte comme « à jour » : le client ne
    /// revient jamais en arrière, quoi qu'annonce le serveur.
    func run() async throws(UpdaterError) -> UpdateResponse? {
        var request = URLRequest(
            url: feed.checkURL(
                currentVersion: currentVersion, channel: channel, architecture: architecture, osVersion: systemVersion
            ),
            cachePolicy: .reloadIgnoringLocalCacheData,
            timeoutInterval: 30
        )
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        if let installID {
            request.setValue(installID.uuidString.lowercased(), forHTTPHeaderField: Self.installIDHeader)
        }

        let (data, http) = try await fetch(request)
        switch http.statusCode {
        case 204:
            return nil
        case 200:
            break
        default:
            throw .server(status: http.statusCode, message: Self.serverMessage(in: data))
        }

        let update: UpdateResponse
        do {
            update = try JSONDecoder().decode(UpdateResponse.self, from: data)
        } catch {
            throw .invalidResponse(Self.describe(error))
        }
        try checkDownloadURL(update.url)
        // Le canal stable ne reçoit jamais de bêta, quoi qu'annonce le serveur. Le canal est celui de
        // la requête : s'il change entre-temps, `Updater` refait la vérification.
        guard channel == .beta || update.channel == .stable else {
            log.error("bêta \(update.version, privacy: .public) proposée au canal stable")
            return nil
        }
        guard update.version > currentVersion else {
            log.error("version \(update.version, privacy: .public) proposée, pas plus récente que \(self.currentVersion, privacy: .public)")
            return nil
        }
        if let minimum = update.minOSVersion, let required = SystemVersion(minimum), required > SystemVersion(systemVersion) {
            log.error("version \(update.version, privacy: .public) proposée, qui exige macOS \(minimum, privacy: .public)")
            return nil
        }
        return update
    }

    /// Statut et corps de la réponse, lu au plus jusqu'à `maximumBodySize` octets et dans le délai
    /// `timeout` au total.
    private func fetch(_ request: URLRequest) async throws(UpdaterError) -> (Data, HTTPURLResponse) {
        let session = session
        let timeout = timeout
        do {
            return try await withThrowingTaskGroup(of: (Data, HTTPURLResponse)?.self) { group in
                group.addTask {
                    let (bytes, response) = try await session.bytes(for: request)
                    guard let http = response as? HTTPURLResponse else {
                        throw UpdaterError.invalidResponse("réponse sans statut HTTP")
                    }
                    var data = Data()
                    for try await byte in bytes {
                        data.append(byte)
                        guard data.count <= Self.maximumBodySize else {
                            throw UpdaterError.invalidResponse("corps de plus de \(Self.maximumBodySize) octets")
                        }
                    }
                    return (data, http)
                }
                group.addTask {
                    try await Task.sleep(for: timeout)
                    return nil
                }
                defer { group.cancelAll() }
                guard let result = try await group.next() ?? nil else {
                    throw UpdaterError.network("pas de réponse en \(timeout.components.seconds) s")
                }
                return result
            }
        } catch let error as UpdaterError {
            throw error
        } catch {
            throw .network(error.localizedDescription)
        }
    }

    /// L'archive vient d'une adresse http(s), et jamais en clair quand la vérification était chiffrée.
    private func checkDownloadURL(_ url: URL) throws(UpdaterError) {
        let scheme = url.scheme?.lowercased()
        let feedIsSecure = feed.baseURL.scheme?.lowercased() == "https"
        guard scheme == "https" || (scheme == "http" && !feedIsSecure), url.host() != nil else {
            throw .invalidResponse("adresse de téléchargement refusée : \(url.absoluteString)")
        }
    }

    /// Le `message` d'un corps d'erreur `{"error": "<code>", "message": "<texte>"}`.
    private static func serverMessage(in data: Data) -> String? {
        struct Body: Decodable {
            let message: String
        }
        return try? JSONDecoder().decode(Body.self, from: data).message
    }

    private static func describe(_ error: any Error) -> String {
        guard let error = error as? DecodingError else { return error.localizedDescription }
        switch error {
        case .keyNotFound(let key, _):
            return "champ « \(key.stringValue) » absent"
        case .typeMismatch(_, let context), .valueNotFound(_, let context), .dataCorrupted(let context):
            let path = context.codingPath.map(\.stringValue).joined(separator: ".")
            return path.isEmpty ? context.debugDescription : "champ « \(path) » : \(context.debugDescription)"
        @unknown default:
            return error.localizedDescription
        }
    }
}
