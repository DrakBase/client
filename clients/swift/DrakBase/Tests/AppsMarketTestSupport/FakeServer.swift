import Foundation
import os

/// Faux serveur DrakBase - Apps Market. Ses sessions ne vont jamais sur le réseau : une classe `URLProtocol`
/// sert leurs requêtes avec les routes déclarées ici. Chaque serveur a son propre nom d'hôte, pour
/// que les tests tournent en parallèle.
public final class FakeServer: Sendable {
    public struct Response: Sendable {
        public var status: Int
        public var headers: [String: String]
        public var body: Data
        /// Livré par morceaux de cette taille, pour faire avancer la progression.
        public var chunkSize: Int
        /// Pause entre deux morceaux : sans elle, URLSession les livre souvent d'un seul bloc.
        public var chunkDelay: TimeInterval

        public init(
            status: Int = 200, headers: [String: String] = [:], body: Data = Data(), chunkSize: Int = 4096,
            chunkDelay: TimeInterval = 0
        ) {
            self.status = status
            self.headers = headers
            self.body = body
            self.chunkSize = chunkSize
            self.chunkDelay = chunkDelay
        }

        public static func json(_ text: String, status: Int = 200) -> Response {
            Response(status: status, headers: ["Content-Type": "application/json"], body: Data(text.utf8))
        }

        public static let noContent = Response(status: 204)
    }

    public typealias Handler = @Sendable (URLRequest) -> Response

    public let host: String
    private let state = OSAllocatedUnfairLock(initialState: State())

    private struct State {
        var routes: [String: Handler] = [:]
        var requests: [URLRequest] = []
    }

    private static let servers = OSAllocatedUnfairLock(initialState: [String: FakeServer]())

    public init() {
        host = "\(UUID().uuidString.lowercased()).appsmarket.test"
        Self.servers.withLock { $0[host] = self }
    }

    public var baseURL: URL {
        URL(string: "https://\(host)")!
    }

    /// Session dont toutes les requêtes sont servies par les faux serveurs.
    public func session() -> URLSession {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [FakeURLProtocol.self]
        return URLSession(configuration: configuration)
    }

    /// Répond à `path` (le chemin de l'URL, sans la requête) par `handler`.
    public func route(_ path: String, _ handler: @escaping Handler) {
        state.withLock { $0.routes[path] = handler }
    }

    public func route(_ path: String, response: Response) {
        route(path) { _ in response }
    }

    /// Requêtes reçues, dans l'ordre.
    public var requests: [URLRequest] {
        state.withLock { $0.requests }
    }

    public func requests(to path: String) -> [URLRequest] {
        requests.filter { $0.url?.path() == path }
    }

    fileprivate static func server(for host: String) -> FakeServer? {
        servers.withLock { $0[host] }
    }

    fileprivate func respond(to request: URLRequest) -> Response {
        let path = request.url?.path() ?? ""
        let handler = state.withLock { state -> Handler? in
            state.requests.append(request)
            return state.routes[path]
        }
        return handler?(request) ?? .json(#"{"error": "not_found", "message": "route inconnue du faux serveur"}"#, status: 404)
    }
}

final class FakeURLProtocol: URLProtocol, @unchecked Sendable {
    override class func canInit(with request: URLRequest) -> Bool {
        true
    }

    override class func canonicalRequest(for request: URLRequest) -> URLRequest {
        request
    }

    override func startLoading() {
        guard let client else { return }
        guard let url = request.url, let host = url.host(), let server = FakeServer.server(for: host) else {
            client.urlProtocol(self, didFailWithError: URLError(.cannotFindHost))
            return
        }
        let response = server.respond(to: request)
        var headers = response.headers
        if headers["Content-Length"] == nil {
            headers["Content-Length"] = String(response.body.count)
        }
        guard let http = HTTPURLResponse(url: url, statusCode: response.status, httpVersion: "HTTP/1.1", headerFields: headers) else {
            client.urlProtocol(self, didFailWithError: URLError(.badServerResponse))
            return
        }
        client.urlProtocol(self, didReceive: http, cacheStoragePolicy: .notAllowed)
        if request.httpMethod != "HEAD" {
            var offset = 0
            while offset < response.body.count {
                if offset > 0, response.chunkDelay > 0 {
                    Thread.sleep(forTimeInterval: response.chunkDelay)
                }
                let end = min(offset + max(response.chunkSize, 1), response.body.count)
                client.urlProtocol(self, didLoad: response.body.subdata(in: offset..<end))
                offset = end
            }
        }
        client.urlProtocolDidFinishLoading(self)
    }

    override func stopLoading() {}
}
