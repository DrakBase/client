@testable import AppsMarketCore
import AppsMarketTestSupport
import Foundation
import os
import Testing

@Suite("Téléchargement de l'archive")
@MainActor
struct ArchiveDownloadTests {
    let world: UpdateWorld

    init() throws {
        world = try UpdateWorld()
    }

    var download: ArchiveDownload {
        ArchiveDownload(session: world.server.session(), directory: world.caches)
    }

    /// Fractions reçues par le rappel de progression.
    final class Progress: Sendable {
        private let values = OSAllocatedUnfairLock(initialState: [Double]())

        var reported: [Double] {
            values.withLock { $0 }
        }

        func record(_ fraction: Double) {
            values.withLock { $0.append(fraction) }
        }
    }

    @Test("Dans Caches, avec une progression croissante jusqu'à 1, puis réutilisée sans nouvelle requête")
    func downloadsWithProgress() async throws {
        defer { world.cleanUp() }
        let publication = try world.publish(version: "1.1.0")
        let progress = Progress()
        let archive = try await download.archive(for: publication.response) { progress.record($0) }

        #expect(archive.deletingLastPathComponent().standardizedFileURL == world.caches.standardizedFileURL)
        #expect(archive.lastPathComponent == "1.1.0.app.tar.gz")
        #expect(try Data(contentsOf: archive) == Data(contentsOf: publication.archive))
        #expect(progress.reported.count > 1)
        #expect(progress.reported == progress.reported.sorted())
        #expect(progress.reported.last == 1)
        // Le téléchargement interrompu ne reste pas.
        #expect(try FileManager.default.contentsOfDirectory(atPath: world.caches.path(percentEncoded: false)) == ["1.1.0.app.tar.gz"])

        let again = try await download.archive(for: publication.response) { _ in }
        #expect(again == archive)
        #expect(world.server.requests(to: publication.downloadPath).count == 1)
    }

    @Test("Une archive plus longue, plus courte ou d'une autre longueur annoncée est refusée")
    func sizeControl() async throws {
        defer { world.cleanUp() }
        let publication = try world.publish(version: "1.1.0")
        let data = try Data(contentsOf: publication.archive)
        let size = UInt64(data.count)

        // Plus d'octets qu'annoncé, sans longueur : refus dès le dépassement.
        world.server.route(publication.downloadPath, response: FakeServer.Response(headers: ["Content-Length": "-1"], body: data + Data(count: 100), chunkSize: 512))
        await #expect(throws: UpdaterError.self) { try await download.archive(for: publication.response) { _ in } }

        world.server.route(publication.downloadPath, response: FakeServer.Response(body: data + Data(count: 10)))
        await #expect(throws: UpdaterError.sizeMismatch(expected: size, received: size + 10)) {
            try await download.archive(for: publication.response) { _ in }
        }

        world.server.route(publication.downloadPath, response: FakeServer.Response(body: data.dropLast(10)))
        await #expect(throws: UpdaterError.sizeMismatch(expected: size, received: size - 10)) {
            try await download.archive(for: publication.response) { _ in }
        }
        #expect((try? FileManager.default.contentsOfDirectory(atPath: world.caches.path(percentEncoded: false)))?.isEmpty ?? true)
    }

    @Test("Le dépassement est refusé pendant la réception, sans attendre la fin")
    func overflowStopsEarly() async throws {
        defer { world.cleanUp() }
        let publication = try world.publish(version: "1.1.0")
        let data = try Data(contentsOf: publication.archive)
        world.server.route(
            publication.downloadPath,
            response: FakeServer.Response(headers: ["Content-Length": "-1"], body: data + Data(count: 4096), chunkSize: 1024, chunkDelay: 0.005)
        )
        let error = await #expect(throws: UpdaterError.self) { try await download.archive(for: publication.response) { _ in } }
        guard case .sizeMismatch(let expected, let received) = error else {
            Issue.record("erreur inattendue : \(String(describing: error))")
            return
        }
        #expect(expected == UInt64(data.count))
        #expect(received > expected && received <= expected + 1024)
    }

    @Test("Empreinte SHA-256 différente, statut d'erreur ou annonce démesurée : refus")
    func otherRefusals() async throws {
        defer { world.cleanUp() }
        let publication = try world.publish(version: "1.1.0")
        var data = try Data(contentsOf: publication.archive)
        data[data.count / 2] ^= 0xFF
        world.server.route(publication.downloadPath, response: FakeServer.Response(body: data))
        await #expect(throws: UpdaterError.checksumMismatch) { try await download.archive(for: publication.response) { _ in } }

        world.server.route(publication.downloadPath, response: FakeServer.Response(status: 410))
        await #expect(throws: UpdaterError.server(status: 410, message: nil)) {
            try await download.archive(for: publication.response) { _ in }
        }

        var json = publication.json
        json["size"] = 3 << 30
        let huge = try JSONDecoder().decode(UpdateResponse.self, from: JSONSerialization.data(withJSONObject: json))
        await #expect(throws: UpdaterError.invalidResponse("archive annoncée de \(UInt64(3) << 30) octets")) {
            try await download.archive(for: huge) { _ in }
        }
        json = publication.json
        json["sha256"] = "9f2c…"
        let unreadable = try JSONDecoder().decode(UpdateResponse.self, from: JSONSerialization.data(withJSONObject: json))
        await #expect(throws: UpdaterError.invalidResponse("empreinte SHA-256 « 9f2c… » illisible")) {
            try await download.archive(for: unreadable) { _ in }
        }
    }

    @Test("Un téléchargement s'arrête si la tâche est annulée")
    func cancellation() async throws {
        defer { world.cleanUp() }
        let publication = try world.publish(version: "1.1.0")
        let download = download
        let response = publication.response
        // Annulée avant même de commencer : la requête est annulée dès son lancement.
        let task = Task {
            try await download.archive(for: response) { _ in }
        }
        task.cancel()
        await #expect(throws: CancellationError.self) { try await task.value }
    }
}
