import AppsMarketCore
import SwiftUI

/// Section « Mises à jour » d'une fenêtre de réglages, à placer dans un `Form` : recherche
/// automatique, installation à la fermeture, statistiques anonymes, canal, « Rechercher
/// maintenant », date du dernier contrôle et version actuelle. Chaque contrôle existe aussi seul,
/// pour un popover.
public struct UpdateSettingsSection: View {
    private let updater: Updater

    public init(updater: Updater) {
        self.updater = updater
    }

    public var body: some View {
        Section("Mises à jour") {
            AutomaticUpdateChecksToggle(updater: updater)
            InstallUpdatesOnQuitToggle(updater: updater)
            if updater.allowsAnonymousStatistics {
                AnonymousStatisticsToggle(updater: updater)
            }
            UpdateChannelPicker(updater: updater)
            LabeledContent("Version actuelle") {
                CurrentVersionText(updater: updater)
            }
            LabeledContent("Dernière vérification") {
                LastUpdateCheckText(updater: updater)
            }
            VStack(alignment: .leading, spacing: 6) {
                CheckForUpdatesButton(updater: updater)
                UpdateStatusText(updater: updater)
            }
        }
    }
}

public struct AutomaticUpdateChecksToggle: View {
    @Bindable private var updater: Updater

    public init(updater: Updater) {
        self.updater = updater
    }

    public var body: some View {
        Toggle("Rechercher automatiquement les mises à jour", isOn: $updater.automaticChecks)
            .disabled(!updater.isConfigured)
    }
}

public struct InstallUpdatesOnQuitToggle: View {
    @Bindable private var updater: Updater

    public init(updater: Updater) {
        self.updater = updater
    }

    public var body: some View {
        Toggle("Installer les mises à jour à la fermeture de l'app", isOn: $updater.installsOnQuit)
            .disabled(!updater.isConfigured)
    }
}

/// Absent de `UpdateSettingsSection` si l'app n'envoie jamais d'identifiant d'installation.
public struct AnonymousStatisticsToggle: View {
    @Bindable private var updater: Updater

    public init(updater: Updater) {
        self.updater = updater
    }

    public var body: some View {
        Toggle(isOn: $updater.sharesAnonymousStatistics) {
            Text("Envoyer des statistiques anonymes")
            Text("Un identifiant tiré au hasard, sans lien avec vous ni avec ce Mac, compte les installations actives.")
        }
        .disabled(!updater.isConfigured || !updater.allowsAnonymousStatistics)
    }
}

public struct UpdateChannelPicker: View {
    @Bindable private var updater: Updater

    public init(updater: Updater) {
        self.updater = updater
    }

    public var body: some View {
        // La bêta voit aussi les versions stables : elle reçoit toujours la plus récente.
        Picker("Canal", selection: $updater.channel) {
            Text("Stable").tag(UpdateChannel.stable)
            Text("Bêta").tag(UpdateChannel.beta)
        }
        .disabled(!updater.isConfigured)
    }
}

/// « Rechercher maintenant » : ouvre aussi la fenêtre de mise à jour, si l'app en a une.
public struct CheckForUpdatesButton: View {
    private let updater: Updater

    public init(updater: Updater) {
        self.updater = updater
    }

    public var body: some View {
        Button("Rechercher maintenant") {
            updater.checkNow()
        }
        .disabled(!updater.canCheckForUpdates)
    }
}

public struct CurrentVersionText: View {
    private let updater: Updater

    public init(updater: Updater) {
        self.updater = updater
    }

    public var body: some View {
        Text(updater.currentVersion?.description ?? "inconnue")
            .textSelection(.enabled)
    }
}

public struct LastUpdateCheckText: View {
    private let updater: Updater

    public init(updater: Updater) {
        self.updater = updater
    }

    public var body: some View {
        if let date = updater.lastCheckDate {
            Text(date, format: .dateTime.day().month(.wide).year().hour().minute())
        } else {
            Text("Jamais")
        }
    }
}

/// État des mises à jour : « À jour », « Version 1.2.0 disponible », ou la raison d'un échec. Rien
/// avant la première vérification.
public struct UpdateStatusText: View {
    private let updater: Updater

    public init(updater: Updater) {
        self.updater = updater
    }

    public var body: some View {
        let summary = UpdateStatus.summary(of: updater)
        if !summary.isEmpty {
            Text(summary)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }
}

/// Textes d'état communs aux réglages et à la fenêtre de mise à jour.
enum UpdateStatus {
    @MainActor
    static func summary(of updater: Updater) -> String {
        let version = updater.availableUpdate.map { "\($0.version)" } ?? ""
        switch updater.phase {
        case .idle:
            return ""
        case .checking:
            return "Recherche en cours…"
        case .upToDate:
            return "À jour"
        case .available:
            return "Version \(version) disponible"
        case .downloading(let progress):
            return "Téléchargement… \(percent(progress))"
        case .ready:
            return updater.installsOnQuit ? "Version \(version) installée à la fermeture" : "Version \(version) prête à installer"
        case .installing:
            return "Installation…"
        case .failed(let error):
            return sentence(error.localizedDescription)
        case .unavailable(let reason):
            return reason.message
        }
    }

    static func percent(_ fraction: Double) -> String {
        fraction.formatted(.percent.precision(.fractionLength(0)))
    }

    /// « de Translate », mais « d'Essai » : élision devant une voyelle.
    static func of(_ name: String) -> String {
        guard let first = name.first, "aàâäeéèêëiîïoôöuùûü".contains(first.lowercased()) else {
            return "de \(name)"
        }
        return "d'\(name)"
    }

    /// Les messages d'erreur du paquet commencent par une minuscule, pour s'insérer dans une phrase.
    static func sentence(_ message: String) -> String {
        guard let first = message.first else { return message }
        let text = first.uppercased() + message.dropFirst()
        return text.hasSuffix(".") ? text : text + "."
    }
}
