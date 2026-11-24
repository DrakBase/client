import AppsMarketCore
import SwiftUI

/// « Rechercher les mises à jour… », juste après « À propos » dans le menu de l'app.
public struct UpdatesCommands: Commands {
    private let updater: Updater

    public init(updater: Updater) {
        self.updater = updater
    }

    public var body: some Commands {
        CommandGroup(after: .appInfo) {
            CheckForUpdatesMenuItem(updater: updater)
        }
    }
}

/// L'élément de menu seul, pour un menu construit à la main.
public struct CheckForUpdatesMenuItem: View {
    private let updater: Updater

    public init(updater: Updater) {
        self.updater = updater
    }

    public var body: some View {
        Button("Rechercher les mises à jour…") {
            updater.checkNow()
        }
        .disabled(!updater.canCheckForUpdates)
    }
}
