@testable import AppsMarketCore
import AppsMarketTestSupport
import Darwin
import Foundation
import Testing

@Suite("Gate de relance")
struct RelaunchGateTests {
    let directory: URL
    let suiteName = "DrakBaseTests.\(UUID().uuidString)"
    let defaults: UserDefaults
    /// L'app qui tourne, et l'ancienne version laissée par l'échange dans le dossier de travail.
    let running: URL
    let previous: URL

    init() throws {
        directory = try FakeApps.temporaryDirectory()
        defaults = try #require(UserDefaults(suiteName: suiteName))
        running = try FakeApps.make(in: directory.appending(path: "Applications"), version: "1.1.0")
        previous = try FakeApps.make(in: directory.appending(path: "work/extract"), version: "1.0.0")
    }

    func cleanUp() {
        defaults.removePersistentDomain(forName: suiteName)
        try? FileManager.default.removeItem(at: directory)
    }

    func run(timeout: Duration = .seconds(20), isRunning: (pid_t) -> Bool = RelaunchGate.isRunning) -> RelaunchGate.Outcome {
        RelaunchGate.run(defaults: defaults, bundle: running, bundleIdentifier: "fr.example.essai", timeout: timeout, isRunning: isRunning)
    }

    /// Processus fils qui dort `seconds` secondes.
    func sleeper(_ seconds: Double) throws -> Process {
        let process = Process()
        process.executableURL = URL(filePath: "/bin/sleep")
        process.arguments = [String(seconds)]
        try process.run()
        return process
    }

    func exists(_ url: URL) -> Bool {
        FileManager.default.fileExists(atPath: url.path(percentEncoded: false))
    }

    @Test("Sans marque, rien à attendre ni à supprimer")
    func noMarker() {
        defer { cleanUp() }
        #expect(run() == .noMarker)
        #expect(exists(previous))
    }

    @Test("L'ancienne instance est déjà fermée : l'ancien bundle et ses dossiers de travail vides disparaissent")
    func previousInstanceGone() throws {
        defer { cleanUp() }
        let finished = try sleeper(0)
        finished.waitUntilExit()
        RelaunchMarker.write(pid: finished.processIdentifier, deadline: Date().addingTimeInterval(20), to: defaults)
        RelaunchMarker.setPreviousBundle(previous, in: defaults)

        #expect(run() == .previousInstanceExited)
        #expect(!exists(previous))
        #expect(!exists(directory.appending(path: "work")))
        #expect(exists(running))
        // La marque ne sert qu'une fois.
        #expect(defaults.object(forKey: RelaunchMarker.key) == nil)
        #expect(defaults.object(forKey: RelaunchMarker.previousBundleKey) == nil)
    }

    @Test("Attend que l'ancienne instance se ferme, avec kill(pid, 0)")
    func waitsForExit() throws {
        defer { cleanUp() }
        let process = try sleeper(0.4)
        RelaunchMarker.write(pid: process.processIdentifier, deadline: Date().addingTimeInterval(20), to: defaults)
        RelaunchMarker.setPreviousBundle(previous, in: defaults)

        let clock = ContinuousClock()
        let start = clock.now
        // Foundation récolte le fils dès sa fin : kill(pid, 0) répond alors ESRCH.
        #expect(run() == .previousInstanceExited)
        #expect(clock.now - start >= .milliseconds(350))
        #expect(!exists(previous))
    }

    @Test("Abandon au bout du délai : l'ancien bundle reste, et sera supprimé au prochain lancement")
    func timeout() throws {
        defer { cleanUp() }
        let process = try sleeper(30)
        defer { process.terminate() }
        RelaunchMarker.write(pid: process.processIdentifier, deadline: Date().addingTimeInterval(20), to: defaults)
        RelaunchMarker.setPreviousBundle(previous, in: defaults)

        #expect(run(timeout: .milliseconds(300)) == .timedOut)
        #expect(exists(previous))
        #expect(defaults.string(forKey: RelaunchMarker.previousBundleKey) != nil)

        // Prochain lancement, sans marque : l'ancienne instance est fermée depuis.
        #expect(run() == .noMarker)
        #expect(!exists(previous))
    }

    @Test("EPERM veut dire « vivant » : launchd n'accepte pas nos signaux mais tourne")
    func permissionDeniedMeansAlive() {
        defer { cleanUp() }
        errno = 0
        #expect(kill(1, 0) == -1 && errno == EPERM)
        #expect(RelaunchGate.isRunning(1))
        #expect(RelaunchGate.isRunning(getpid()))
        RelaunchMarker.write(pid: 1, deadline: Date().addingTimeInterval(20), to: defaults)
        #expect(run(timeout: .milliseconds(200)) == .timedOut)
    }

    @Test("Marque périmée ou illisible : pas d'attente ; marque de ce processus : pas d'attente non plus")
    func staleMarkers() throws {
        defer { cleanUp() }
        RelaunchMarker.write(pid: 1, deadline: Date().addingTimeInterval(-1), to: defaults)
        #expect(run(timeout: .milliseconds(200)) == .expiredMarker)

        defaults.set(["pid": "pas un nombre"], forKey: RelaunchMarker.key)
        #expect(run(timeout: .milliseconds(200)) == .noMarker)
        #expect(defaults.object(forKey: RelaunchMarker.key) == nil)

        RelaunchMarker.write(pid: getpid(), deadline: Date().addingTimeInterval(20), to: defaults)
        #expect(run(timeout: .milliseconds(200)) == .noMarker)
    }

    @Test("Seul un ancien bundle de cette app, qui n'est pas celui qui tourne, est supprimé")
    func cleanupGuards() throws {
        defer { cleanUp() }
        let stranger = try FakeApps.make(in: directory.appending(path: "autre"), bundleIdentifier: "fr.example.autre", version: "1.0.0")
        RelaunchMarker.setPreviousBundle(stranger, in: defaults)
        #expect(run() == .noMarker)
        #expect(exists(stranger))

        RelaunchMarker.setPreviousBundle(running, in: defaults)
        #expect(run() == .noMarker)
        #expect(exists(running))

        defaults.set("chemin/relatif/Essai.app", forKey: RelaunchMarker.previousBundleKey)
        #expect(run() == .noMarker)
        #expect(defaults.object(forKey: RelaunchMarker.previousBundleKey) == nil)
    }
}
