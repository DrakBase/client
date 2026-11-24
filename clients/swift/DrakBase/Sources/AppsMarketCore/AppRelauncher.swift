import AppKit
import OSLog
import os

private let log = Logger(subsystem: "DrakBase", category: "relaunch")

/// Relance de l'app après l'échange des bundles. Le code livré n'en a qu'une, `WorkspaceRelauncher` ;
/// les tests la remplacent (`@testable import`), puisqu'elle ferme l'app.
@MainActor
protocol AppRelauncher {
    /// Ouvre la nouvelle instance de `app`, puis ferme celle-ci. Ne revient que si la relance a échoué.
    func relaunch(_ app: URL) async throws(UpdaterError)
}

/// Relance par NSWorkspace, comme l'a validé l'essai M0 (`spikes/m0-relaunch`).
@MainActor
struct WorkspaceRelauncher: AppRelauncher {
    /// Temps laissé à macOS pour créer la nouvelle instance.
    static let launchTimeout: Duration = .seconds(10)

    func relaunch(_ app: URL) async throws(UpdaterError) {
        let configuration = NSWorkspace.OpenConfiguration()
        configuration.createsNewApplicationInstance = true
        configuration.activates = true
        let launchError = OSAllocatedUnfairLock<String?>(initialState: nil)
        // Ce bloc n'est appelé qu'une fois la nouvelle instance lancée, or elle attend notre
        // fermeture dans le gate : l'attendre bloquerait les deux instances (constaté en M0). Il ne
        // sert qu'à repérer un échec immédiat.
        NSWorkspace.shared.openApplication(at: app, configuration: configuration) { _, error in
            if let error {
                launchError.withLock { $0 = error.localizedDescription }
            }
        }

        // On attend seulement que macOS ait créé la nouvelle instance, puis on quitte. Sans nouvelle
        // instance, l'app reste ouverte : mieux vaut l'ancienne version que plus rien.
        let own = ProcessInfo.processInfo.processIdentifier
        let identifier = Bundle.main.bundleIdentifier ?? ""
        let clock = ContinuousClock()
        let deadline = clock.now + Self.launchTimeout
        while true {
            if let message = launchError.withLock({ $0 }) {
                throw .relaunch(message)
            }
            let other = NSRunningApplication.runningApplications(withBundleIdentifier: identifier)
                .first { $0.processIdentifier != own }
            if let other {
                log.notice("nouvelle instance visible, pid \(other.processIdentifier)")
                break
            }
            guard clock.now < deadline else {
                throw .relaunch("la nouvelle instance n'est pas apparue")
            }
            try? await Task.sleep(for: .milliseconds(50))
        }
        NSApplication.shared.terminate(nil)
        // `terminate` revient quand l'app reporte sa fermeture, ou la refuse. Si elle tourne encore
        // une fois le gate de la nouvelle instance abandonné, elle a refusé de se fermer.
        try? await Task.sleep(for: RelaunchGate.timeout + .seconds(5))
        throw .relaunch("l'app ne s'est pas fermée")
    }
}
