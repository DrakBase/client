import Darwin
import Foundation
import OSLog

private let log = Logger(subsystem: "DrakBase", category: "gate")

/// Jamais deux instances en même temps après une mise à jour (`docs/PLAN.md`, « Déroulé », étape 7).
///
/// L'ancienne instance laisse une marque dans les préférences, ouvre la nouvelle, puis quitte dès que
/// macOS l'a créée. La nouvelle attend ici que l'ancienne soit fermée, puis supprime l'ancien bundle.
public enum RelaunchGate {
    /// Attente maximale de l'ancienne instance, et durée de validité de la marque.
    static let timeout: Duration = .seconds(20)

    /// À appeler en tout premier dans le `main` de l'app, avant toute autre initialisation : une base
    /// de données ouverte dès l'`init` d'un `@State` le serait déjà par l'ancienne instance.
    ///
    /// Sans marque, ne fait rien. Si l'ancienne instance tourne encore au bout de 20 s, celle-ci
    /// quitte (`exit(0)`) et la laisse tourner.
    public static func waitForPreviousInstance() {
        let outcome = run(
            defaults: .standard,
            bundle: Bundle.main.bundleURL,
            bundleIdentifier: Bundle.main.bundleIdentifier,
            timeout: timeout,
            isRunning: isRunning
        )
        if outcome == .timedOut {
            exit(0)
        }
    }

    enum Outcome: Equatable {
        case noMarker
        /// La marque date de plus de 20 s : l'ancienne instance n'attend plus rien de celle-ci.
        case expiredMarker
        case previousInstanceExited
        case timedOut
    }

    /// Le travail du gate, avec ce que les tests remplacent : les préférences, l'app et le temps.
    static func run(
        defaults: UserDefaults,
        bundle: URL,
        bundleIdentifier: String?,
        timeout: Duration,
        now: () -> Date = Date.init,
        isRunning: (pid_t) -> Bool
    ) -> Outcome {
        var outcome = Outcome.noMarker
        if let marker = RelaunchMarker.take(from: defaults) {
            if marker.deadline < now() {
                outcome = .expiredMarker
            } else if marker.pid != getpid() {
                let clock = ContinuousClock()
                let start = clock.now
                while isRunning(marker.pid) {
                    if clock.now - start > timeout {
                        log.error("l'instance \(marker.pid) tourne encore après \(timeout), abandon")
                        return .timedOut
                    }
                    Thread.sleep(forTimeInterval: 0.05)
                }
                log.notice("instance \(marker.pid) fermée après \(clock.now - start)")
                outcome = .previousInstanceExited
            }
        }
        // L'ancienne instance est fermée : son bundle peut partir.
        removePreviousBundle(defaults: defaults, bundle: bundle, bundleIdentifier: bundleIdentifier)
        return outcome
    }

    /// `kill(pid, 0)` dit si le processus existe ; `EPERM` veut dire qu'il existe mais qu'on ne peut
    /// pas lui envoyer de signal (sandbox, autre utilisateur) : il est donc vivant.
    static func isRunning(_ pid: pid_t) -> Bool {
        kill(pid, 0) == 0 || errno == EPERM
    }

    /// Supprime l'ancien bundle laissé par l'installation, ou le met à la corbeille si la
    /// suppression est refusée, puis ses dossiers de travail devenus vides. Seul un bundle de cette
    /// app, qui n'est pas celui qui tourne, est touché.
    static func removePreviousBundle(defaults: UserDefaults, bundle: URL, bundleIdentifier: String?) {
        guard let previous = RelaunchMarker.takePreviousBundle(from: defaults) else { return }
        let fileManager = FileManager.default
        guard fileManager.fileExists(atPath: previous.path(percentEncoded: false)) else { return }
        guard let bundleIdentifier, (try? BundleInfo(at: previous))?.identifier == bundleIdentifier,
              previous.resolvingSymlinksInPath().standardizedFileURL != bundle.resolvingSymlinksInPath().standardizedFileURL
        else {
            log.error("ancien bundle ignoré : \(previous.path(percentEncoded: false), privacy: .public)")
            return
        }
        do {
            try fileManager.removeItem(at: previous)
        } catch {
            log.error("suppression de l'ancien bundle impossible, mise à la corbeille : \(error.localizedDescription, privacy: .public)")
            try? fileManager.trashItem(at: previous, resultingItemURL: nil)
        }
        // rmdir ne supprime qu'un dossier vide : `extract` ou `previous`, puis le dossier de travail.
        let parent = previous.deletingLastPathComponent()
        for directory in [parent, parent.deletingLastPathComponent()] {
            _ = directory.withUnsafeFileSystemRepresentation { path in rmdir(path) }
        }
    }
}

/// Ce que l'ancienne instance laisse à la nouvelle dans les préférences de l'app. Les clés gardent
/// l'ancien nom du paquet : une version d'avant le renommage relance une version d'après.
enum RelaunchMarker {
    /// `{pid, deadline}` de l'ancienne instance, valable 20 s.
    static let key = "AppsMarketKit.relaunch"
    /// Chemin de l'ancien bundle, à supprimer quand plus aucune instance n'en tourne.
    static let previousBundleKey = "AppsMarketKit.previousBundle"

    struct Marker: Equatable {
        let pid: pid_t
        let deadline: Date
    }

    static func write(pid: pid_t, deadline: Date, to defaults: UserDefaults) {
        defaults.set(["pid": Int(pid), "deadline": deadline] as [String: Any], forKey: key)
        // La nouvelle instance la lit dès son lancement, dans un autre processus.
        defaults.synchronize()
    }

    /// Lit la marque et l'efface : elle ne sert qu'une fois.
    static func take(from defaults: UserDefaults) -> Marker? {
        guard let marker = defaults.dictionary(forKey: key) else { return nil }
        defaults.removeObject(forKey: key)
        guard let pid = (marker["pid"] as? NSNumber)?.int32Value, pid > 0,
              let deadline = marker["deadline"] as? Date
        else {
            return nil
        }
        return Marker(pid: pid, deadline: deadline)
    }

    static func setPreviousBundle(_ url: URL, in defaults: UserDefaults) {
        defaults.set(url.path(percentEncoded: false), forKey: previousBundleKey)
        defaults.synchronize()
    }

    static func takePreviousBundle(from defaults: UserDefaults) -> URL? {
        guard let path = defaults.string(forKey: previousBundleKey) else { return nil }
        defaults.removeObject(forKey: previousBundleKey)
        return path.hasPrefix("/") ? URL(filePath: path, directoryHint: .isDirectory) : nil
    }
}
