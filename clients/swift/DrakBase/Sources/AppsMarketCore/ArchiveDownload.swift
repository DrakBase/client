import CryptoKit
import Foundation
import os

/// Téléchargement de l'archive d'une mise à jour dans Caches (`docs/PLAN.md`, « Déroulé », étape 2).
/// La taille est contrôlée pendant la réception, l'empreinte SHA-256 à la fin.
struct ArchiveDownload: Sendable {
    /// Bien au-delà de la limite du serveur (1 Gio par défaut) : une annonce plus grosse est refusée
    /// avant le moindre octet.
    static let maximumSize: UInt64 = 2 << 30
    /// Durée maximale d'un téléchargement : de quoi passer une grosse archive sur une connexion lente,
    /// sans laisser un serveur qui traîne bloquer indéfiniment les opérations suivantes.
    static let resourceTimeout: TimeInterval = 4 * 3600

    /// Session dont la configuration est reprise, avec ses classes de protocole dans les tests.
    let session: URLSession
    /// `Caches/<identifiant de l'app>/AppsMarketKit`.
    let directory: URL

    /// L'archive de cette version, déjà complète dans Caches, ou téléchargée maintenant.
    ///
    /// - Parameter progress: fraction reçue, de 0 à 1, appelée au plus une centaine de fois.
    /// - Throws: `UpdaterError`, ou `CancellationError`.
    @concurrent
    func archive(for update: UpdateResponse, progress: @escaping @Sendable (Double) -> Void) async throws -> URL {
        guard update.size <= Self.maximumSize else {
            throw UpdaterError.invalidResponse("archive annoncée de \(update.size) octets")
        }
        let expectedDigest = update.sha256.lowercased()
        guard expectedDigest.count == 64, expectedDigest.utf8.allSatisfy(\.isHexDigit) else {
            throw UpdaterError.invalidResponse("empreinte SHA-256 « \(update.sha256) » illisible")
        }
        let destination = archiveURL(for: update.version)
        if Self.isComplete(destination, size: update.size, digest: expectedDigest) {
            progress(1)
            return destination
        }
        do {
            try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        } catch {
            throw UpdaterError.storage(error.localizedDescription)
        }
        removeArchives(keeping: nil)

        let partial = directory.appending(path: "\(update.version).app.tar.gz.part", directoryHint: .notDirectory)
        guard FileManager.default.createFile(atPath: partial.path(percentEncoded: false), contents: nil) else {
            throw UpdaterError.storage("création de \(partial.lastPathComponent) impossible")
        }
        let digest: String
        do {
            let file = try FileHandle(forWritingTo: partial)
            defer { try? file.close() }
            digest = try await receive(update, into: file, progress: progress)
        } catch {
            try? FileManager.default.removeItem(at: partial)
            switch error {
            case let error as UpdaterError: throw error
            case let error as CancellationError: throw error
            default: throw UpdaterError.storage(error.localizedDescription)
            }
        }
        guard digest == expectedDigest else {
            try? FileManager.default.removeItem(at: partial)
            throw UpdaterError.checksumMismatch
        }
        do {
            try FileManager.default.moveItem(at: partial, to: destination)
        } catch {
            try? FileManager.default.removeItem(at: partial)
            throw UpdaterError.storage(error.localizedDescription)
        }
        return destination
    }

    /// Emplacement de l'archive d'une version, une fois complète.
    func archiveURL(for version: SemanticVersion) -> URL {
        directory.appending(path: "\(version).app.tar.gz", directoryHint: .notDirectory)
    }

    /// Supprime les archives et les téléchargements interrompus, sauf `keeping`.
    func removeArchives(keeping kept: URL?) {
        let entries = (try? FileManager.default.contentsOfDirectory(at: directory, includingPropertiesForKeys: nil)) ?? []
        for entry in entries where entry.lastPathComponent != kept?.lastPathComponent {
            try? FileManager.default.removeItem(at: entry)
        }
    }

    /// Reçoit le corps dans `file` et rend son empreinte SHA-256 en hexadécimal.
    private func receive(_ update: UpdateResponse, into file: FileHandle, progress: @escaping @Sendable (Double) -> Void) async throws -> String {
        let receiver = Receiver(expectedSize: update.size, file: file, progress: progress)
        let queue = OperationQueue()
        queue.maxConcurrentOperationCount = 1
        let configuration = session.configuration
        configuration.timeoutIntervalForResource = min(configuration.timeoutIntervalForResource, Self.resourceTimeout)
        let downloadSession = URLSession(configuration: configuration, delegate: receiver, delegateQueue: queue)
        defer { downloadSession.finishTasksAndInvalidate() }
        var request = URLRequest(url: update.url, cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: 60)
        request.setValue("application/octet-stream", forHTTPHeaderField: "Accept")
        let task = downloadSession.dataTask(with: request)
        return try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                receiver.start(task, continuation: continuation)
            }
        } onCancel: {
            task.cancel()
        }
    }

    /// Une archive déjà téléchargée sert si elle a la taille et l'empreinte annoncées.
    private static func isComplete(_ url: URL, size: UInt64, digest: String) -> Bool {
        let attributes = try? FileManager.default.attributesOfItem(atPath: url.path(percentEncoded: false))
        guard (attributes?[.size] as? NSNumber)?.uint64Value == size,
              let file = try? FileHandle(forReadingFrom: url)
        else {
            return false
        }
        defer { try? file.close() }
        var hasher = SHA256()
        while let chunk = try? file.read(upToCount: 1 << 20), !chunk.isEmpty {
            hasher.update(data: chunk)
        }
        return hex(hasher.finalize()) == digest
    }

    static func hex(_ digest: SHA256.Digest) -> String {
        digest.map { String(format: "%02x", $0) }.joined()
    }
}

/// Délégué du téléchargement. La session l'appelle sur sa file, une méthode à la fois ; l'état
/// partagé avec la tâche qui attend est protégé par un verrou.
private final class Receiver: NSObject, URLSessionDataDelegate, Sendable {
    private struct State {
        var continuation: CheckedContinuation<String, any Error>?
        /// Fin arrivée avant `start`, quand la tâche était annulée avant même d'être lancée.
        var earlyResult: Result<String, any Error>?
        var hasher = SHA256()
        var received: UInt64 = 0
        var reported = 0
        /// Refus décidé par le délégué, qui annule alors la tâche.
        var failure: UpdaterError?
    }

    private let expectedSize: UInt64
    private let file: FileHandle
    private let progress: @Sendable (Double) -> Void
    private let state = OSAllocatedUnfairLock(uncheckedState: State())

    init(expectedSize: UInt64, file: FileHandle, progress: @escaping @Sendable (Double) -> Void) {
        self.expectedSize = expectedSize
        self.file = file
        self.progress = progress
    }

    func start(_ task: URLSessionDataTask, continuation: CheckedContinuation<String, any Error>) {
        let earlyResult = state.withLockUnchecked { state -> Result<String, any Error>? in
            guard let result = state.earlyResult else {
                state.continuation = continuation
                return nil
            }
            return result
        }
        if let earlyResult {
            continuation.resume(with: earlyResult)
        } else {
            task.resume()
        }
    }

    func urlSession(
        _ session: URLSession, dataTask: URLSessionDataTask, didReceive response: URLResponse,
        completionHandler: @escaping @Sendable (URLSession.ResponseDisposition) -> Void
    ) {
        let status = (response as? HTTPURLResponse)?.statusCode ?? 0
        var failure: UpdaterError?
        if status != 200 {
            failure = .server(status: status, message: nil)
        } else if response.expectedContentLength >= 0, UInt64(response.expectedContentLength) != expectedSize {
            failure = .sizeMismatch(expected: expectedSize, received: UInt64(response.expectedContentLength))
        }
        if let failure {
            state.withLockUnchecked { $0.failure = failure }
            completionHandler(.cancel)
        } else {
            completionHandler(.allow)
        }
    }

    func urlSession(_ session: URLSession, dataTask: URLSessionDataTask, didReceive data: Data) {
        let accepted = state.withLockUnchecked { state -> Bool in
            guard state.failure == nil else { return false }
            state.received += UInt64(data.count)
            guard state.received <= expectedSize else {
                state.failure = .sizeMismatch(expected: expectedSize, received: state.received)
                return false
            }
            state.hasher.update(data: data)
            return true
        }
        guard accepted else {
            dataTask.cancel()
            return
        }
        do {
            try file.write(contentsOf: data)
        } catch {
            state.withLockUnchecked { $0.failure = .storage(error.localizedDescription) }
            dataTask.cancel()
            return
        }
        report()
    }

    func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: (any Error)?) {
        // Le résultat est rendu à la continuation, ou gardé pour `start` si elle n'est pas encore
        // rangée : les deux sous le même verrou.
        let (continuation, result) = state.withLockUnchecked { state -> (CheckedContinuation<String, any Error>?, Result<String, any Error>) in
            let result = outcome(of: state, error: error)
            guard let continuation = state.continuation else {
                state.earlyResult = result
                return (nil, result)
            }
            state.continuation = nil
            return (continuation, result)
        }
        continuation?.resume(with: result)
    }

    private func outcome(of state: State, error: (any Error)?) -> Result<String, any Error> {
        if let failure = state.failure {
            return .failure(failure)
        }
        if let error {
            if (error as? URLError)?.code == .cancelled {
                return .failure(CancellationError())
            }
            return .failure(UpdaterError.network(error.localizedDescription))
        }
        guard state.received == expectedSize else {
            return .failure(UpdaterError.sizeMismatch(expected: expectedSize, received: state.received))
        }
        return .success(ArchiveDownload.hex(state.hasher.finalize()))
    }

    /// Progression par pas d'un pour cent, pour ne pas inonder l'acteur principal.
    private func report() {
        let step = state.withLockUnchecked { state -> Int? in
            let fraction = expectedSize == 0 ? 1 : Double(state.received) / Double(expectedSize)
            let step = Int(fraction * 100)
            guard step > state.reported else { return nil }
            state.reported = step
            return step
        }
        if let step {
            progress(Double(step) / 100)
        }
    }
}

private extension UInt8 {
    var isHexDigit: Bool {
        (UInt8(ascii: "0")...UInt8(ascii: "9")).contains(self) || (UInt8(ascii: "a")...UInt8(ascii: "f")).contains(self)
    }
}
