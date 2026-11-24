import AppKit
import AppsMarketCore
import SwiftUI

/// Fenêtre de mise à jour : une fenêtre AppKit avec du SwiftUI, qui fonctionne aussi pour une app de
/// la barre des menus. Elle suit l'état de l'updater : recherche, version proposée et ses notes,
/// progression, échec.
///
/// ```swift
/// updater.presenter = UpdatePrompt(updater: updater)
/// ```
@MainActor
public final class UpdatePrompt: UpdatePresenting {
    private weak var updater: Updater?
    private let appName: String
    private var window: NSWindow?

    /// - Parameter appName: nom affiché, par défaut celui du bundle de l'app.
    public init(updater: Updater, appName: String? = nil) {
        self.updater = updater
        self.appName = appName ?? Self.bundleName
    }

    public func presentUpdates(for updater: Updater) {
        show()
    }

    /// Montre la fenêtre devant les autres, et active l'app : une app de la barre des menus n'est
    /// pas au premier plan.
    public func show() {
        guard let updater else { return }
        let window = window ?? makeWindow(for: updater)
        self.window = window
        if !window.isVisible {
            window.center()
        }
        NSApplication.shared.activate()
        window.makeKeyAndOrderFront(nil)
    }

    public func close() {
        window?.close()
    }

    private func makeWindow(for updater: Updater) -> NSWindow {
        let content = UpdatePromptView(updater: updater, appName: appName) { [weak self] in
            self?.close()
        }
        let controller = NSHostingController(rootView: content)
        controller.sizingOptions = [.preferredContentSize]
        let window = NSWindow(contentViewController: controller)
        window.styleMask = [.titled, .closable]
        window.title = "Mise à jour \(UpdateStatus.of(appName))"
        window.isReleasedWhenClosed = false
        window.collectionBehavior.insert(.moveToActiveSpace)
        return window
    }

    private static var bundleName: String {
        let info = Bundle.main.infoDictionary ?? [:]
        return info["CFBundleDisplayName"] as? String ?? info["CFBundleName"] as? String ?? ProcessInfo.processInfo.processName
    }
}

/// Contenu de la fenêtre de mise à jour.
struct UpdatePromptView: View {
    let updater: Updater
    let appName: String
    let close: () -> Void

    var body: some View {
        HStack(alignment: .top, spacing: 16) {
            Image(nsImage: NSApplication.shared.applicationIconImage)
                .resizable()
                .frame(width: 64, height: 64)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 12) {
                Text(title)
                    .font(.headline)
                if let detail {
                    Text(detail)
                        .fixedSize(horizontal: false, vertical: true)
                }
                if showsNotes, let notes = updater.availableUpdate?.notes, !notes.isEmpty {
                    ReleaseNotesView(markdown: notes)
                        .frame(minHeight: 120, maxHeight: 260)
                }
                progress
                buttons
            }
        }
        .padding(20)
        .frame(width: 520)
    }

    private var newVersion: String {
        updater.availableUpdate.map { "\($0.version)" } ?? ""
    }

    private var currentVersion: String {
        updater.currentVersion?.description ?? "inconnue"
    }

    private var title: String {
        switch updater.phase {
        case .idle, .checking:
            "Recherche de mises à jour…"
        case .upToDate:
            "\(appName) est à jour"
        case .available, .ready, .downloading:
            "Une nouvelle version \(UpdateStatus.of(appName)) est disponible"
        case .installing:
            "Installation \(UpdateStatus.of(appName)) \(newVersion)…"
        case .failed(.relaunch):
            "Relance impossible"
        case .failed:
            "La mise à jour a échoué"
        case .unavailable:
            updater.availableUpdate == nil ? "Mises à jour indisponibles" : "\(appName) \(newVersion) est disponible"
        }
    }

    private var detail: String? {
        switch updater.phase {
        case .idle, .checking, .installing:
            nil
        case .upToDate:
            "La version \(currentVersion) est la plus récente."
        case .available, .downloading:
            "\(appName) \(newVersion) est disponible ; vous avez la version \(currentVersion)."
        case .ready:
            "\(appName) \(newVersion) est prête à être installée ; vous avez la version \(currentVersion)."
        case .failed(let error):
            UpdateStatus.sentence(error.localizedDescription)
        case .unavailable(let reason):
            reason.message
        }
    }

    private var showsNotes: Bool {
        switch updater.phase {
        case .available, .downloading, .ready, .unavailable: true
        case .idle, .checking, .upToDate, .installing, .failed: false
        }
    }

    @ViewBuilder
    private var progress: some View {
        switch updater.phase {
        case .idle, .checking:
            ProgressView()
                .progressViewStyle(.linear)
        case .downloading(let fraction):
            ProgressView(value: fraction) {
                Text("Téléchargement… \(UpdateStatus.percent(fraction))")
            }
        case .installing:
            ProgressView {
                Text("Installation, puis relance…")
            }
            .progressViewStyle(.linear)
        default:
            EmptyView()
        }
    }

    @ViewBuilder
    private var buttons: some View {
        HStack {
            switch updater.phase {
            case .available, .downloading, .ready:
                if let update = updater.availableUpdate {
                    Button("Ignorer cette version") {
                        updater.skip(update.version)
                        close()
                    }
                }
                Spacer()
                Button("Plus tard", action: close)
                    .keyboardShortcut(.cancelAction)
                Button("Installer et relancer") {
                    updater.installAndRelaunch()
                }
                .keyboardShortcut(.defaultAction)
                .disabled(updater.isBusy)
            case .unavailable:
                if let update = updater.availableUpdate {
                    Button("Ignorer cette version") {
                        updater.skip(update.version)
                        close()
                    }
                }
                Spacer()
                Button("OK", action: close)
                    .keyboardShortcut(.defaultAction)
            case .failed:
                Spacer()
                Button("Réessayer") {
                    updater.retry()
                }
                Button("OK", action: close)
                    .keyboardShortcut(.defaultAction)
            case .idle, .checking, .upToDate:
                Spacer()
                Button("OK", action: close)
                    .keyboardShortcut(.defaultAction)
            case .installing:
                EmptyView()
            }
        }
    }
}
