import AppKit
import AppsMarketCore
@testable import AppsMarketUI
import Foundation
import SwiftUI
import Testing

@Suite("Interface de mise à jour")
@MainActor
struct UpdateInterfaceTests {
    /// Updater d'un bundle sans configuration : il reste indisponible, sans rien lancer.
    let updater = Updater(bundle: Bundle(for: BundleMarker.self))

    @Test("Messages d'état en français, phrases complètes pour la fenêtre")
    func statusTexts() {
        #expect(UpdateStatus.sentence("serveur de mises à jour injoignable : hors ligne") == "Serveur de mises à jour injoignable : hors ligne.")
        #expect(UpdateStatus.sentence("Déjà une phrase.") == "Déjà une phrase.")
        #expect(UpdateStatus.sentence("") == "")
        #expect(UpdateStatus.percent(0.42).hasPrefix("42"))
        #expect(UpdateStatus.of("Translate") == "de Translate")
        #expect(UpdateStatus.of("Essai") == "d'Essai")
        #expect(UpdateStatus.of("Écrire") == "d'Écrire")
        #expect(UpdateStatus.of("") == "de ")
        #expect(UpdateStatus.summary(of: updater).hasPrefix("Les mises à jour ne sont pas configurées dans cette app"))
    }

    @Test("Réglages, menu et fenêtre se construisent et se dessinent")
    func viewsRender() {
        let settings = NSHostingView(rootView: Form { UpdateSettingsSection(updater: updater) }.formStyle(.grouped))
        settings.frame = NSRect(x: 0, y: 0, width: 480, height: 400)
        settings.layoutSubtreeIfNeeded()
        #expect(settings.fittingSize.height > 0)

        let prompt = NSHostingView(rootView: UpdatePromptView(updater: updater, appName: "Essai") {})
        prompt.layoutSubtreeIfNeeded()
        #expect(prompt.fittingSize.width >= 500)

        let notes = NSHostingView(rootView: ReleaseNotesView(markdown: "## Nouveautés\n- Plus rapide"))
        notes.layoutSubtreeIfNeeded()
        #expect(notes.fittingSize.height > 0)

        _ = UpdatesCommands(updater: updater).body
        _ = UpdatePrompt(updater: updater, appName: "Essai")
    }
}

private final class BundleMarker {}
