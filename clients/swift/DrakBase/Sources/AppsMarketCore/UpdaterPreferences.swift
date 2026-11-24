import Foundation

/// Ce que l'updater garde dans les préférences de l'app, d'un lancement à l'autre : réglages,
/// identifiant d'installation, version ignorée, échec d'une installation à la fermeture et
/// préparation en cours.
struct UpdaterPreferences {
    /// Clés des préférences de l'app. Elles gardent l'ancien nom du paquet : les renommer ferait
    /// perdre leurs réglages aux installations existantes.
    enum Keys {
        static let automaticChecks = "AppsMarketKit.automaticChecks"
        static let checkInterval = "AppsMarketKit.checkInterval"
        static let installsOnQuit = "AppsMarketKit.installsOnQuit"
        static let channel = "AppsMarketKit.channel"
        static let sharesAnonymousStatistics = "AppsMarketKit.sharesAnonymousStatistics"
        static let installID = "AppsMarketKit.installID"
        static let lastCheckDate = "AppsMarketKit.lastCheckDate"
        static let skippedVersion = "AppsMarketKit.skippedVersion"
        static let preparedUpdate = "AppsMarketKit.preparedUpdate"
        /// `{version, message}` d'une installation à la fermeture qui a échoué.
        static let quitInstallFailure = "AppsMarketKit.quitInstallFailure"
    }

    let defaults: UserDefaults

    // MARK: - Réglages

    var automaticChecks: Bool {
        get { defaults.object(forKey: Keys.automaticChecks) as? Bool ?? true }
        nonmutating set { defaults.set(newValue, forKey: Keys.automaticChecks) }
    }

    /// `nil` tant que l'app ne l'a pas réglé.
    var checkInterval: TimeInterval? {
        get { defaults.object(forKey: Keys.checkInterval) as? Double }
        nonmutating set { defaults.set(newValue, forKey: Keys.checkInterval) }
    }

    var installsOnQuit: Bool {
        get { defaults.object(forKey: Keys.installsOnQuit) as? Bool ?? false }
        nonmutating set { defaults.set(newValue, forKey: Keys.installsOnQuit) }
    }

    var channel: UpdateChannel {
        get { defaults.string(forKey: Keys.channel).flatMap(UpdateChannel.init(rawValue:)) ?? .stable }
        nonmutating set { defaults.set(newValue.rawValue, forKey: Keys.channel) }
    }

    var sharesAnonymousStatistics: Bool {
        get { defaults.object(forKey: Keys.sharesAnonymousStatistics) as? Bool ?? true }
        nonmutating set { defaults.set(newValue, forKey: Keys.sharesAnonymousStatistics) }
    }

    // MARK: - Vérifications

    /// Date de la dernière vérification réussie.
    var lastCheckDate: Date? {
        get { defaults.object(forKey: Keys.lastCheckDate) as? Date }
        nonmutating set { defaults.set(newValue, forKey: Keys.lastCheckDate) }
    }

    var skippedVersion: SemanticVersion? {
        get { defaults.string(forKey: Keys.skippedVersion).flatMap { try? SemanticVersion.parseRelease($0) } }
        nonmutating set { defaults.set(newValue?.description, forKey: Keys.skippedVersion) }
    }

    // MARK: - Identifiant d'installation

    /// Identifiant d'installation, créé à la première demande : un UUID v4 aléatoire.
    func installID() -> UUID {
        if let stored = defaults.string(forKey: Keys.installID), let id = UUID(uuidString: stored) {
            return id
        }
        let id = UUID()
        defaults.set(id.uuidString.lowercased(), forKey: Keys.installID)
        return id
    }

    /// Sans statistiques, l'identifiant disparaît : en les rouvrant, on en tire un nouveau.
    func forgetInstallID() {
        defaults.removeObject(forKey: Keys.installID)
    }

    // MARK: - Installation à la fermeture

    /// Raison de l'échec de l'installation de `version` à la fermeture précédente. L'échec d'une
    /// autre version est oublié.
    func quitInstallFailure(for version: SemanticVersion) -> String? {
        guard let failure = defaults.dictionary(forKey: Keys.quitInstallFailure) else { return nil }
        guard failure["version"] as? String == version.description, let message = failure["message"] as? String else {
            clearQuitInstallFailure()
            return nil
        }
        return message
    }

    func recordQuitInstallFailure(of version: SemanticVersion, message: String) {
        defaults.set(["version": version.description, "message": message], forKey: Keys.quitInstallFailure)
    }

    func clearQuitInstallFailure() {
        defaults.removeObject(forKey: Keys.quitInstallFailure)
    }

    // MARK: - Préparation gardée d'un lancement à l'autre

    struct Preparation: Codable {
        let update: UpdateResponse
        let prepared: PreparedUpdate
        let preparedAt: Date
    }

    /// La préparation gardée, ou `nil`. Une préparation illisible est effacée à la lecture.
    var preparation: Preparation? {
        get {
            guard let data = defaults.data(forKey: Keys.preparedUpdate) else { return nil }
            guard let stored = try? JSONDecoder().decode(Preparation.self, from: data) else {
                defaults.removeObject(forKey: Keys.preparedUpdate)
                return nil
            }
            return stored
        }
        nonmutating set {
            guard let newValue else {
                defaults.removeObject(forKey: Keys.preparedUpdate)
                return
            }
            guard let data = try? JSONEncoder().encode(newValue) else { return }
            defaults.set(data, forKey: Keys.preparedUpdate)
        }
    }
}
