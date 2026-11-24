import DrakBase
import Foundation
import SwiftUI
import Testing

/// Ce qu'écrit une app compilée comme Translate et Pli pour intégrer le paquet : le gate dans `main`,
/// l'updater, son délégué, la fenêtre, les réglages et le menu. Tout doit compiler sans le moindre
/// avertissement avec l'isolation MainActor par défaut.
@Suite("Updater depuis du code isolé sur le MainActor")
struct UpdaterClientTests {
    final class AppDelegate: UpdaterDelegate {
        private(set) var savedDraft = false

        func updaterShouldRelaunch(_ updater: Updater) async -> Bool {
            savedDraft = true
            return true
        }
    }

    final class Presenter: UpdatePresenting {
        private(set) var presented = 0

        func presentUpdates(for updater: Updater) {
            presented += 1
        }
    }

    struct SettingsView: View {
        let updater: Updater

        var body: some View {
            Form {
                UpdateSettingsSection(updater: updater)
                Section("Popover") {
                    AutomaticUpdateChecksToggle(updater: updater)
                    InstallUpdatesOnQuitToggle(updater: updater)
                    AnonymousStatisticsToggle(updater: updater)
                    UpdateChannelPicker(updater: updater)
                    CheckForUpdatesButton(updater: updater)
                    LastUpdateCheckText(updater: updater)
                    CurrentVersionText(updater: updater)
                    UpdateStatusText(updater: updater)
                    CheckForUpdatesMenuItem(updater: updater)
                }
            }
        }
    }

    /// L'exemple d'intégration du README, compilé tel quel (sans `@main`, et jamais lancé).
    enum Main {
        static func main() {
            RelaunchGate.waitForPreviousInstance()
            DemoApp.main()
        }
    }

    struct DemoApp: App {
        @State private var updater: Updater = {
            let updater = Updater()
            updater.presenter = UpdatePrompt(updater: updater)
            updater.start()
            return updater
        }()

        var body: some Scene {
            WindowGroup { Text("Essai") }
                .commands { UpdatesCommands(updater: updater) }
            Settings {
                Form { UpdateSettingsSection(updater: updater) }.formStyle(.grouped)
            }
        }
    }

    @Test("Gate, updater, délégué, présentation, réglages et menu")
    func integration() async throws {
        MainActor.preconditionIsolated("ce module doit être isolé sur le MainActor par défaut")
        // Sans marque de relance dans les préférences, le gate rend la main aussitôt.
        RelaunchGate.waitForPreviousInstance()

        // Le bundle des tests n'a pas de configuration : l'updater reste indisponible.
        let updater = Updater(bundle: Bundle(for: AppDelegate.self))
        let delegate = AppDelegate()
        let presenter = Presenter()
        updater.delegate = delegate
        updater.presenter = presenter
        updater.start()
        #expect(!updater.isConfigured)
        guard case .unavailable(.misconfigured) = updater.phase else {
            Issue.record("phase inattendue : \(updater.phase)")
            return
        }

        await updater.checkNow().value
        #expect(presenter.presented == 1)
        await updater.download().value
        await updater.installAndRelaunch().value
        #expect(!delegate.savedDraft)
        // Référencée sans être appelée : elle écrirait dans les préférences de l'outil de test.
        let skip: (SemanticVersion) -> Void = updater.skip
        _ = skip

        _ = SettingsView(updater: updater).body
        _ = UpdatesCommands(updater: updater)
        updater.presenter = UpdatePrompt(updater: updater)
    }
}
