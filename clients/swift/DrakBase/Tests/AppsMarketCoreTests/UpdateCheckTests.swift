@testable import AppsMarketCore
import AppsMarketTestSupport
import Foundation
import Testing

@Suite("Vérification auprès d'un faux serveur")
@MainActor
struct UpdateCheckTests {
    let world: UpdateWorld

    init() throws {
        world = try UpdateWorld()
    }

    func check(
        channel: UpdateChannel = .stable,
        installID: UUID? = nil,
        systemVersion: OperatingSystemVersion = OperatingSystemVersion(majorVersion: 15, minorVersion: 1, patchVersion: 0),
        feed: UpdateFeed? = nil
    ) throws -> UpdateCheck {
        UpdateCheck(
            feed: try feed ?? world.feed,
            session: world.server.session(),
            currentVersion: world.installedVersion,
            channel: channel,
            installID: installID,
            architecture: .current,
            systemVersion: systemVersion
        )
    }

    @Test("204 : à jour")
    func upToDate() async throws {
        defer { world.cleanUp() }
        world.withdrawAll()
        #expect(try await check().run() == nil)
    }

    @Test("200 : la mise à jour, avec l'adresse, les paramètres et les en-têtes du protocole")
    func available() async throws {
        defer { world.cleanUp() }
        let publication = try world.publish(version: "1.1.0", channel: .beta)
        let installID = UUID()
        let update = try await check(channel: .beta, installID: installID).run()
        #expect(update == publication.response)

        let request = try #require(world.server.requests(to: world.checkPath).first)
        let query = try #require(request.url.flatMap { URLComponents(url: $0, resolvingAgainstBaseURL: false) }?.queryItems)
        #expect(query == [
            URLQueryItem(name: "bundle_type", value: "app"),
            URLQueryItem(name: "channel", value: "beta"),
            URLQueryItem(name: "os_version", value: "15.1"),
        ])
        #expect(request.value(forHTTPHeaderField: "X-AppsMarket-Install-Id") == installID.uuidString.lowercased())
        #expect(request.value(forHTTPHeaderField: "Accept") == "application/json")
        #expect(request.cachePolicy == .reloadIgnoringLocalCacheData)
    }

    @Test("Sans statistiques, aucun identifiant ne part, ni en en-tête ni dans l'adresse")
    func noInstallID() async throws {
        defer { world.cleanUp() }
        world.withdrawAll()
        _ = try await check(installID: nil).run()
        let request = try #require(world.server.requests.first)
        #expect(request.value(forHTTPHeaderField: "X-AppsMarket-Install-Id") == nil)
        #expect(request.url?.query()?.contains("install") == false)
    }

    @Test("Le client ne revient jamais en arrière, et écarte une version qui exige un macOS plus récent")
    func neverBackwards() async throws {
        defer { world.cleanUp() }
        try world.publish(version: "1.0.0")
        #expect(try await check().run() == nil)
        try world.publish(version: "0.9.0")
        #expect(try await check().run() == nil)

        try world.publish(version: "1.1.0", minimumSystemVersion: "15.2")
        #expect(try await check().run() == nil)
        let compatible = try await check(systemVersion: OperatingSystemVersion(majorVersion: 15, minorVersion: 2, patchVersion: 0)).run()
        #expect(compatible?.version.description == "1.1.0")
    }

    @Test("min_os_version se lit comme côté serveur : nombres de tête gardés, minimum illisible ignoré")
    func minimumSystemVersionReading() async throws {
        defer { world.cleanUp() }
        // « 15.2-beta » vaut 15.2 : écartée sur macOS 15.1, proposée sur 15.2.
        try world.publish(version: "1.1.0", minimumSystemVersion: "15.2-beta")
        #expect(try await check().run() == nil)
        let compatible = try await check(systemVersion: OperatingSystemVersion(majorVersion: 15, minorVersion: 2, patchVersion: 0)).run()
        #expect(compatible?.version.description == "1.1.0")

        // Illisible : rien n'est écarté ici, comme côté serveur. C'est l'installeur qui refusera le
        // bundle, avant tout échange (`LocalInstallerTests`).
        try world.publish(version: "1.1.0", minimumSystemVersion: "quatorze")
        #expect(try await check().run()?.minOSVersion == "quatorze")
    }

    @Test("Statut d'erreur : le message du serveur est repris ; 410 dit que la version est retirée")
    func serverErrors() async throws {
        defer { world.cleanUp() }
        world.server.route(world.checkPath, response: .json(#"{"error": "not_found", "message": "app « essai » inconnue"}"#, status: 404))
        await #expect(throws: UpdaterError.server(status: 404, message: "app « essai » inconnue")) {
            try await check().run()
        }
        world.server.route(world.checkPath, response: FakeServer.Response(status: 500, body: Data("panne".utf8)))
        await #expect(throws: UpdaterError.server(status: 500, message: nil)) {
            try await check().run()
        }
        #expect(UpdaterError.server(status: 404, message: "app « essai » inconnue").localizedDescription
            == "le serveur de mises à jour a répondu 404 : app « essai » inconnue")
        #expect(UpdaterError.server(status: 410, message: nil).localizedDescription == "cette version a été retirée par son éditeur")
    }

    @Test("Réponse hors contrat : champ manquant, JSON illisible, adresse de téléchargement refusée")
    func invalidResponses() async throws {
        defer { world.cleanUp() }
        let publication = try world.publish(version: "1.1.0")
        func serve(_ change: (inout [String: Any]) -> Void) throws {
            var json = publication.json
            change(&json)
            world.server.route(world.checkPath, response: FakeServer.Response(body: try JSONSerialization.data(withJSONObject: json)))
        }

        try serve { $0["signature"] = nil }
        await #expect(throws: UpdaterError.invalidResponse("champ « signature » absent")) { try await check().run() }

        world.server.route(world.checkPath, response: .json("{"))
        let error = await #expect(throws: UpdaterError.self) { try await check().run() }
        guard case .invalidResponse = error else {
            Issue.record("erreur inattendue : \(String(describing: error))")
            return
        }

        for url in ["file:///etc/passwd", "http://\(world.server.host)/dl/x/Essai.app.tar.gz", "ftp://maj.example.fr/a.tar.gz"] {
            try serve { $0["url"] = url }
            await #expect(throws: UpdaterError.invalidResponse("adresse de téléchargement refusée : \(url)"), "« \(url) »") {
                try await check().run()
            }
        }
        // Une vérification en clair (serveur local) peut télécharger en clair.
        let local = try UpdateFeed(baseURL: #require(URL(string: "http://\(world.server.host)/api/v1/update/essai")))
        try serve { $0["url"] = "http://\(world.server.host)\(publication.downloadPath)" }
        #expect(try await check(feed: local).run()?.version.description == "1.1.0")
    }

    @Test("Le canal stable ne reçoit jamais de bêta, quoi que réponde le serveur")
    func stableNeverGetsBeta() async throws {
        defer { world.cleanUp() }
        try world.publish(version: "1.1.0-beta.1", channel: .beta)
        #expect(try await check(channel: .stable).run() == nil)
        #expect(try await check(channel: .beta).run()?.version.description == "1.1.0-beta.1")
    }

    @Test("Réponse trop longue ou trop lente : refusée sans tout lire, ni attendre indéfiniment")
    func boundedResponse() async throws {
        defer { world.cleanUp() }
        world.server.route(world.checkPath, response: FakeServer.Response(body: Data(count: UpdateCheck.maximumBodySize + 1), chunkSize: 65_536))
        await #expect(throws: UpdaterError.invalidResponse("corps de plus de \(UpdateCheck.maximumBodySize) octets")) {
            try await check().run()
        }

        world.server.route(world.checkPath) { _ in
            Thread.sleep(forTimeInterval: 3)
            return .noContent
        }
        var slow = try check()
        slow.timeout = .seconds(1)
        let clock = ContinuousClock()
        let start = clock.now
        await #expect(throws: UpdaterError.network("pas de réponse en 1 s")) { try await slow.run() }
        #expect(clock.now - start < .seconds(2.5))
    }

    @Test("Serveur injoignable : erreur réseau")
    func unreachable() async throws {
        defer { world.cleanUp() }
        let feed = try UpdateFeed(baseURL: #require(URL(string: "https://inconnu.appsmarket.test/api/v1/update/essai")))
        let error = await #expect(throws: UpdaterError.self) { try await check(feed: feed).run() }
        guard case .network = error else {
            Issue.record("erreur inattendue : \(String(describing: error))")
            return
        }
    }
}
