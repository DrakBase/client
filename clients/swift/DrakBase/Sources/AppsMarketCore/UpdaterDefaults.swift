import Foundation

/// Ce que l'updater d'une vraie app reçoit, hors des tests : son installeur, ses préférences, son
/// dossier de Caches et sa session.
extension Updater {
    static func defaultInstaller(app: InstalledApp, configuration: UpdaterConfiguration) -> any UpdateInstaller {
        if let service = configuration.installerService {
            return UnsupportedInstaller(service: service)
        }
        return LocalInstaller(app: app, configuration: configuration)
    }

    static func standardEnvironment(bundleIdentifier: String?) -> Environment {
        let caches = (try? FileManager.default.url(for: .cachesDirectory, in: .userDomainMask, appropriateFor: nil, create: true))
            ?? FileManager.default.temporaryDirectory
        return Environment(
            defaults: .standard,
            session: makeSession(),
            cachesDirectory: caches
                .appending(path: bundleIdentifier ?? "AppsMarketKit", directoryHint: .isDirectory)
                .appending(path: "AppsMarketKit", directoryHint: .isDirectory),
            notificationCenter: .default,
            relauncher: WorkspaceRelauncher()
        )
    }

    /// Session éphémère, sans cookies ni cache : rien ne relie deux vérifications, sauf
    /// l'identifiant d'installation quand l'utilisateur l'accepte.
    static func makeSession() -> URLSession {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.requestCachePolicy = .reloadIgnoringLocalCacheData
        configuration.urlCache = nil
        configuration.httpCookieStorage = nil
        configuration.httpShouldSetCookies = false
        configuration.timeoutIntervalForRequest = 60
        return URLSession(configuration: configuration)
    }
}
