import AppKit
import AppsMarketCrypto
import Foundation
import Observation
import OSLog

private let log = Logger(subsystem: "DrakBase", category: "updater")

/// Présente l'état des mises à jour à l'utilisateur, comme la fenêtre `UpdatePrompt`
/// d'`AppsMarketUI`.
@MainActor
public protocol UpdatePresenting: AnyObject {
    /// Appelé par `checkNow()`, puis quand une vérification automatique trouve une version à
    /// proposer. L'état à montrer est celui de `updater`, qui continue d'évoluer.
    func presentUpdates(for updater: Updater)
}

@MainActor
public protocol UpdaterDelegate: AnyObject {
    /// Appelé juste avant d'installer et de relancer, une fois la mise à jour prête : l'app peut y
    /// enregistrer un brouillon, ou refuser (`false`), et la mise à jour reste prête. Après `true`,
    /// l'app doit accepter de se fermer : sinon la nouvelle instance attend en vain dans le gate.
    func updaterShouldRelaunch(_ updater: Updater) async -> Bool
}

extension UpdaterDelegate {
    public func updaterShouldRelaunch(_ updater: Updater) async -> Bool {
        true
    }
}

/// Mises à jour d'une app Swift auprès du serveur DrakBase - Apps Market : vérification, téléchargement dans
/// Caches, vérification de la signature, préparation, échange des bundles et relance
/// (`docs/PLAN.md`, « Paquet Swift »).
///
/// Une seule instance par app, créée après `RelaunchGate.waitForPreviousInstance()`, puis
/// `start()`. La configuration vient de l'`Info.plist` signé de l'app, les réglages de ses
/// préférences.
@MainActor
@Observable
public final class Updater {
    public enum Phase: Equatable, Sendable {
        /// Rien de fait depuis le lancement, ou version ignorée.
        case idle
        case checking
        case upToDate
        /// `availableUpdate` est proposée, pas encore téléchargée.
        case available
        case downloading(progress: Double)
        /// Téléchargée, vérifiée et préparée : l'installation n'est plus qu'un échange.
        case ready
        case installing
        case failed(UpdaterError)
        case unavailable(UpdateUnavailability)
    }

    /// Intervalle par défaut entre deux vérifications automatiques : un jour.
    public static let defaultCheckInterval: TimeInterval = 86_400
    /// Bornes de l'intervalle, quoi que contiennent les préférences : une heure et trente jours.
    public static let minimumCheckInterval: TimeInterval = 3_600
    public static let maximumCheckInterval: TimeInterval = 30 * 86_400
    /// Au-delà, une préparation est refaite depuis l'archive de Caches : macOS vide les dossiers
    /// temporaires au démarrage, et en retire les fichiers inutilisés depuis trois jours.
    static let preparationLifetime: TimeInterval = 86_400

    public private(set) var phase: Phase
    /// Dernière version proposée par le serveur. Elle reste connue pendant le téléchargement,
    /// l'installation, et après un échec.
    public private(set) var availableUpdate: UpdateResponse?
    /// Date de la dernière vérification réussie.
    public private(set) var lastCheckDate: Date?
    public private(set) var skippedVersion: SemanticVersion?
    /// Version de l'app qui tourne, `nil` si son `Info.plist` est illisible.
    public let currentVersion: SemanticVersion?
    /// Faux pour une app qui n'envoie jamais d'identifiant d'installation, comme Pli.
    public let allowsAnonymousStatistics: Bool

    public var automaticChecks: Bool {
        didSet {
            guard automaticChecks != oldValue else { return }
            preferences.automaticChecks = automaticChecks
            scheduleChecks(checkingNow: false)
        }
    }

    /// Intervalle entre deux vérifications automatiques, en secondes, ramené entre une heure et
    /// trente jours ; une valeur qui n'est pas un nombre fini vaut un jour.
    public var checkInterval: TimeInterval {
        didSet {
            guard checkInterval != oldValue else { return }
            preferences.checkInterval = checkInterval
            scheduleChecks(checkingNow: false)
        }
    }

    /// Une mise à jour trouvée est téléchargée et préparée en silence, puis installée à la
    /// fermeture de l'app, sans relance.
    public var installsOnQuit: Bool {
        didSet {
            guard installsOnQuit != oldValue else { return }
            preferences.installsOnQuit = installsOnQuit
        }
    }

    public var channel: UpdateChannel {
        didSet {
            guard channel != oldValue else { return }
            preferences.channel = channel
            // Revenue au canal stable, l'app ne propose plus une bêta déjà trouvée.
            guard channel == .stable, availableUpdate?.channel == .beta, phase != .installing, installedUpdate == nil,
                  case .success(let setup) = setup
            else {
                return
            }
            discardPreparation(setup)
            availableUpdate = nil
            phase = .idle
        }
    }

    /// Envoi de l'identifiant d'installation, pour les statistiques anonymes. Toujours faux si
    /// l'app ne les permet pas.
    public var sharesAnonymousStatistics: Bool {
        didSet {
            guard sharesAnonymousStatistics != oldValue, allowsAnonymousStatistics else { return }
            preferences.sharesAnonymousStatistics = sharesAnonymousStatistics
            if !sharesAnonymousStatistics {
                preferences.forgetInstallID()
            }
        }
    }

    @ObservationIgnored public weak var delegate: (any UpdaterDelegate)?
    /// Retenu par l'updater, comme le reste de l'interface de l'app.
    @ObservationIgnored public var presenter: (any UpdatePresenting)?

    /// Vrai si l'`Info.plist` de l'app contient une configuration complète.
    public var isConfigured: Bool {
        if case .success = setup { return true }
        return false
    }

    /// Vrai pendant une vérification, un téléchargement ou une installation.
    public var isBusy: Bool {
        switch phase {
        case .checking, .downloading, .installing: true
        case .idle, .upToDate, .available, .ready, .failed, .unavailable: false
        }
    }

    /// Faux aussi une fois la mise à jour échangée dans ce processus : il ne reste qu'à relancer.
    public var canCheckForUpdates: Bool {
        isConfigured && !isBusy && installedUpdate == nil
    }

    // MARK: - Dépendances

    /// Ce que les tests remplacent (`@testable import`) : préférences, réseau, dossiers, horloge,
    /// relance.
    struct Environment {
        var defaults: UserDefaults
        var session: URLSession
        /// `Caches/<identifiant de l'app>/AppsMarketKit`.
        var cachesDirectory: URL
        var notificationCenter: NotificationCenter
        var relauncher: any AppRelauncher
        var architecture: CPUArchitecture = .current
        var systemVersion: OperatingSystemVersion = ProcessInfo.processInfo.operatingSystemVersion
        var now: @Sendable () -> Date = { Date() }
        var sleep: @Sendable (Duration) async throws -> Void = { try await Task.sleep(for: $0) }
    }

    struct Setup {
        let configuration: UpdaterConfiguration
        let app: InstalledApp
        let installer: any UpdateInstaller
    }

    private enum FailedOperation {
        case check
        case installation
    }

    /// Clés des préférences de l'app, rangées avec elles dans `UpdaterPreferences`.
    typealias Keys = UpdaterPreferences.Keys

    @ObservationIgnored private let setup: Result<Setup, ConfigurationError>
    @ObservationIgnored private let environment: Environment
    @ObservationIgnored private let preferences: UpdaterPreferences
    @ObservationIgnored private var isStarted = false
    @ObservationIgnored private var prepared: PreparedUpdate?
    @ObservationIgnored private var preparedAt: Date?
    /// Mise à jour échangée dans ce processus, qui tourne encore sur l'ancienne version : il n'y a
    /// plus rien à vérifier ni à installer, seulement à relancer.
    private var installedUpdate: InstalledUpdate?
    @ObservationIgnored private var lastCheckAttempt: Date?
    /// Ce qui a mis l'updater en échec, et que `retry()` refait.
    @ObservationIgnored private var failedOperation: FailedOperation?
    @ObservationIgnored private var scheduler: Task<Void, Never>?
    /// Dernière opération lancée : chaque nouvelle attend la précédente.
    @ObservationIgnored private var lastOperation: Task<Void, Never>?
    @ObservationIgnored private var terminationObservation: NotificationObservation?

    /// Mises à jour de l'app d'un bundle, celle qui tourne par défaut. Sans configuration complète
    /// dans son `Info.plist`, l'updater reste `unavailable`.
    ///
    /// - Parameters:
    ///   - installer: par défaut `LocalInstaller`, qui travaille dans le processus de l'app.
    ///   - allowsAnonymousStatistics: faux pour ne jamais créer ni envoyer d'identifiant
    ///     d'installation.
    public convenience init(
        bundle: Bundle = .main,
        installer: (any UpdateInstaller)? = nil,
        allowsAnonymousStatistics: Bool = true
    ) {
        let setup: Result<Setup, ConfigurationError>
        do {
            let configuration = try UpdaterConfiguration(bundle: bundle)
            let app = try InstalledApp(bundle: bundle)
            setup = .success(Setup(
                configuration: configuration,
                app: app,
                installer: installer ?? Self.defaultInstaller(app: app, configuration: configuration)
            ))
        } catch {
            setup = .failure(error)
        }
        self.init(
            setup: setup,
            allowsAnonymousStatistics: allowsAnonymousStatistics,
            environment: Self.standardEnvironment(bundleIdentifier: bundle.bundleIdentifier)
        )
    }

    init(setup: Result<Setup, ConfigurationError>, allowsAnonymousStatistics: Bool, environment: Environment) {
        self.setup = setup
        self.environment = environment
        self.allowsAnonymousStatistics = allowsAnonymousStatistics
        let preferences = UpdaterPreferences(defaults: environment.defaults)
        self.preferences = preferences
        automaticChecks = preferences.automaticChecks
        checkInterval = preferences.checkInterval ?? Self.defaultCheckInterval
        installsOnQuit = preferences.installsOnQuit
        channel = preferences.channel
        sharesAnonymousStatistics = allowsAnonymousStatistics && preferences.sharesAnonymousStatistics
        lastCheckDate = preferences.lastCheckDate
        skippedVersion = preferences.skippedVersion
        switch setup {
        case .success(let setup):
            currentVersion = setup.app.version
            phase = .idle
        case .failure(let error):
            currentVersion = nil
            phase = .unavailable(.misconfigured(error.localizedDescription))
        }
    }

    deinit {
        scheduler?.cancel()
    }

    // MARK: - API

    /// Reprend une mise à jour déjà préparée, puis vérifie tout de suite et à intervalle si la
    /// vérification automatique est active. Sans effet la deuxième fois.
    public func start() {
        guard !isStarted, case .success(let setup) = setup else { return }
        isStarted = true
        restorePreparation(setup)
        // L'identifiant est créé à la première ouverture, s'il doit l'être.
        _ = installIDForRequest()
        observeTermination()
        scheduleChecks(checkingNow: true)
    }

    /// Vérification demandée par l'utilisateur : la présentation s'ouvre aussitôt, et une version
    /// ignorée est proposée quand même.
    @discardableResult
    public func checkNow() -> Task<Void, Never> {
        presenter?.presentUpdates(for: self)
        if phase == .checking, let lastOperation {
            return lastOperation
        }
        return enqueue { [weak self] in
            await self?.check(userInitiated: true)
        }
    }

    /// Télécharge, vérifie et prépare `availableUpdate`.
    @discardableResult
    public func download() -> Task<Void, Never> {
        if case .downloading = phase, let lastOperation {
            return lastOperation
        }
        return enqueue { [weak self] in
            await self?.performDownload()
        }
    }

    /// Télécharge et prépare la mise à jour si besoin, demande l'accord du délégué, échange les
    /// bundles, puis relance l'app.
    @discardableResult
    public func installAndRelaunch() -> Task<Void, Never> {
        enqueue { [weak self] in
            await self?.performInstallAndRelaunch()
        }
    }

    /// Refait ce qui a échoué : la vérification si c'est elle, ou s'il ne reste aucune version à
    /// installer (version retirée) ; sinon l'installation, ou la seule relance si l'échange est déjà
    /// fait. Sans échec en cours, ne fait rien.
    @discardableResult
    public func retry() -> Task<Void, Never> {
        guard case .failed = phase else { return lastOperation ?? Task {} }
        if failedOperation == .check || (availableUpdate == nil && installedUpdate == nil) {
            return checkNow()
        }
        return installAndRelaunch()
    }

    /// N'avertit plus de cette version sans qu'on le demande. Une préparation de cette version est
    /// abandonnée.
    public func skip(_ version: SemanticVersion) {
        skippedVersion = version
        preferences.skippedVersion = version
        preferences.clearQuitInstallFailure()
        guard availableUpdate?.version == version, phase != .installing, installedUpdate == nil,
              case .success(let setup) = setup
        else {
            return
        }
        discardPreparation(setup)
        availableUpdate = nil
        phase = .idle
    }

    // MARK: - Vérification

    private func scheduleChecks(checkingNow: Bool) {
        scheduler?.cancel()
        scheduler = nil
        // Une mise à jour déjà échangée dans ce processus : il n'y a plus rien à vérifier.
        guard isStarted, automaticChecks, isConfigured, installedUpdate == nil else { return }
        let sleep = environment.sleep
        scheduler = Task { [weak self] in
            var force = checkingNow
            while !Task.isCancelled {
                guard let delay = self?.delayBeforeNextCheck(force: force) else { return }
                if delay > .zero {
                    do {
                        try await sleep(delay)
                    } catch {
                        return
                    }
                }
                guard let self, !Task.isCancelled, installedUpdate == nil else { return }
                // Comptée comme une tentative même si la vérification est écartée (une autre
                // opération en cours) : la boucle ne tourne jamais sans pause.
                lastCheckAttempt = environment.now()
                await enqueue { [weak self] in
                    await self?.check(userInitiated: false)
                }.value
                force = false
            }
        }
    }

    /// Temps restant avant la prochaine vérification automatique, compté depuis la dernière
    /// tentative, réussie ou non, ou depuis celle d'un autre lancement.
    private func delayBeforeNextCheck(force: Bool) -> Duration {
        let interval = effectiveCheckInterval
        guard !force, let last = [lastCheckAttempt, lastCheckDate].compactMap({ $0 }).max() else { return .zero }
        // Une date future (horloge reculée) ne repousse pas au-delà d'un intervalle.
        let remaining = min(max(interval - environment.now().timeIntervalSince(last), 0), interval)
        return .seconds(remaining)
    }

    private var effectiveCheckInterval: TimeInterval {
        guard checkInterval.isFinite else { return Self.defaultCheckInterval }
        return min(max(checkInterval, Self.minimumCheckInterval), Self.maximumCheckInterval)
    }

    private func check(userInitiated: Bool) async {
        guard case .success(let setup) = setup, !isBusy, installedUpdate == nil else { return }
        let update: UpdateResponse?
        do {
            update = try await latestUpdate(setup)
        } catch {
            log.error("vérification : \(error.localizedDescription, privacy: .public)")
            // Une vérification automatique qui échoue ne fait pas oublier une version déjà trouvée.
            let known = prepared != nil || availableUpdate != nil
            if !userInitiated, known {
                phase = restingPhase(setup)
            } else {
                setFailed(error, during: .check)
            }
            return
        }
        lastCheckDate = environment.now()
        preferences.lastCheckDate = lastCheckDate
        apply(update, setup: setup, userInitiated: userInitiated)
    }

    /// La réponse du serveur pour le canal suivi. Si le canal change pendant la requête, la réponse
    /// valait pour l'ancien : la vérification est refaite avec le nouveau.
    private func latestUpdate(_ setup: Setup) async throws(UpdaterError) -> UpdateResponse? {
        while true {
            let requested = channel
            phase = .checking
            lastCheckAttempt = environment.now()
            let check = UpdateCheck(
                feed: setup.configuration.feed,
                session: environment.session,
                currentVersion: setup.app.version,
                channel: requested,
                installID: installIDForRequest(),
                architecture: environment.architecture,
                systemVersion: environment.systemVersion
            )
            let update = try await check.run()
            if channel == requested {
                return update
            }
        }
    }

    private func apply(_ update: UpdateResponse?, setup: Setup, userInitiated: Bool) {
        guard let update else {
            // Rien de plus récent : une version préparée que le serveur ne propose plus, retirée
            // par exemple, n'est pas installée.
            preferences.clearQuitInstallFailure()
            discardPreparation(setup)
            availableUpdate = nil
            phase = .upToDate
            return
        }
        if let prepared, prepared.version != update.version {
            discardPreparation(setup)
        } else if let preparedAt, environment.now().timeIntervalSince(preparedAt) > Self.preparationLifetime {
            // Refaite depuis l'archive de Caches, sans nouveau téléchargement.
            discardPreparation(setup, keepingArchive: true)
        }
        availableUpdate = update
        phase = restingPhase(setup)
        let skipped = !userInitiated && update.version == skippedVersion
        guard !skipped else { return }
        if phase == .available, let failure = preferences.quitInstallFailure(for: update.version) {
            // L'installation à la fermeture a échoué : la version passe désormais par la fenêtre, où
            // l'utilisateur voit pourquoi et peut réessayer.
            setFailed(.installationAtQuit(failure), during: .installation)
            if !userInitiated {
                presenter?.presentUpdates(for: self)
            }
            return
        }
        if installsOnQuit {
            if phase == .available {
                download()
            }
            return
        }
        guard !userInitiated else { return }
        switch phase {
        case .available, .ready:
            presenter?.presentUpdates(for: self)
        case .unavailable(let reason) where reason.isFixableByMovingTheApp:
            presenter?.presentUpdates(for: self)
        default:
            break
        }
    }

    /// Phase de repos, d'après ce qui est connu : version préparée, version proposée, ou rien.
    private func restingPhase(_ setup: Setup) -> Phase {
        if prepared != nil { return .ready }
        guard availableUpdate != nil else { return .idle }
        if let reason = setup.installer.unavailability() { return .unavailable(reason) }
        return .available
    }

    /// Identifiant d'installation à envoyer, créé à la première demande : un UUID v4 aléatoire,
    /// rangé dans les préférences. `nil` si les statistiques sont coupées.
    private func installIDForRequest() -> UUID? {
        guard allowsAnonymousStatistics, sharesAnonymousStatistics else { return nil }
        return preferences.installID()
    }

    // MARK: - Téléchargement et préparation

    private var downloader: ArchiveDownload {
        ArchiveDownload(session: environment.session, directory: environment.cachesDirectory)
    }

    private func performDownload() async {
        guard case .success(let setup) = setup, prepared == nil, installedUpdate == nil, let update = availableUpdate, !isBusy else {
            return
        }
        if let reason = setup.installer.unavailability() {
            phase = .unavailable(reason)
            return
        }
        phase = .downloading(progress: 0)
        do {
            let archive = try await downloader.archive(for: update) { [weak self] fraction in
                Task { @MainActor in
                    self?.reportProgress(fraction, for: update)
                }
            }
            // Vérifiée dans l'app pour échouer tôt ; l'installeur vérifie de nouveau sa propre copie.
            do {
                let signature = try MinisignSignature(tauri: update.signature)
                try await setup.configuration.publicKeys.verifyFile(at: archive, signature: signature)
                    .requireVersion(update.version)
            } catch {
                // L'archive refusée ne resservira pas : la prochaine tentative la téléchargera de nouveau.
                downloader.removeArchives(keeping: nil)
                throw error
            }
            let prepared = try await setup.installer.prepare(archive: archive, for: update)
            guard availableUpdate == update else {
                // Ignorée, ou remplacée par une autre version, pendant le téléchargement.
                setup.installer.discard(prepared)
                return
            }
            let now = environment.now()
            self.prepared = prepared
            preparedAt = now
            preferences.preparation = UpdaterPreferences.Preparation(update: update, prepared: prepared, preparedAt: now)
            phase = .ready
        } catch {
            guard availableUpdate == update else { return }
            log.error("téléchargement de \(update.version, privacy: .public) : \(error.localizedDescription, privacy: .public)")
            if case UpdaterError.server(status: 410, _) = error {
                // Retirée par son éditeur depuis la vérification : elle ne reviendra pas. Elle est
                // oubliée, avec ce qui en reste dans Caches, et la suite est une nouvelle
                // vérification, pas un nouveau téléchargement de la même adresse.
                downloader.removeArchives(keeping: nil)
                availableUpdate = nil
            }
            fail(error, setup: setup)
        }
    }

    private func reportProgress(_ fraction: Double, for update: UpdateResponse) {
        guard case .downloading(let current) = phase, availableUpdate == update, fraction > current else { return }
        phase = .downloading(progress: fraction)
    }

    private func fail(_ error: any Error, setup: Setup) {
        switch error {
        case is CancellationError:
            phase = restingPhase(setup)
        case let error as UpdaterError:
            setFailed(error, during: .installation)
        case let error as SignatureError:
            setFailed(.signature(error), during: .installation)
        case let error as InstallationError:
            if case .unavailable(let reason) = error {
                phase = .unavailable(reason)
            } else {
                setFailed(.installation(error), during: .installation)
            }
        default:
            setFailed(.storage(error.localizedDescription), during: .installation)
        }
    }

    private func setFailed(_ error: UpdaterError, during operation: FailedOperation) {
        failedOperation = operation
        phase = .failed(error)
    }

    // MARK: - Installation

    private func performInstallAndRelaunch() async {
        guard case .success(let setup) = setup, !isBusy else { return }
        // Déjà échangée dans ce processus : seule la relance reste à refaire.
        if installedUpdate != nil {
            await relaunch(setup)
            return
        }
        // L'utilisateur réessaie lui-même : un échec à la fermeture ne compte plus.
        preferences.clearQuitInstallFailure()
        if prepared == nil {
            await performDownload()
        }
        guard let prepared else { return }
        if let delegate, !(await delegate.updaterShouldRelaunch(self)) {
            log.notice("relance refusée par l'app")
            return
        }
        // Ignorée ou remplacée pendant que le délégué répondait.
        guard self.prepared == prepared, !isBusy else { return }
        phase = .installing
        let installed: InstalledUpdate
        do {
            installed = try await Self.install(prepared, with: setup.installer)
        } catch {
            log.error("installation de \(prepared.version, privacy: .public) : \(error.localizedDescription, privacy: .public)")
            discardPreparation(setup)
            fail(error, setup: setup)
            return
        }
        finishInstallation(installed)
        installedUpdate = installed
        // La vérification automatique s'arrête : si la relance échoue, l'app reste ouverte sur
        // l'ancienne version, et chaque vérification serait écartée aussitôt.
        scheduleChecks(checkingNow: false)
        await relaunch(setup)
    }

    private func relaunch(_ setup: Setup) async {
        phase = .installing
        // La marque doit précéder la nouvelle instance, qui la lit dès son lancement. Elle reste
        // après un échec : une instance qui apparaîtrait en retard attendrait encore celle-ci, puis
        // abandonnerait, plutôt que de tourner en même temps qu'elle. Elle s'éteint d'elle-même.
        RelaunchMarker.write(
            pid: ProcessInfo.processInfo.processIdentifier,
            deadline: environment.now().addingTimeInterval(TimeInterval(RelaunchGate.timeout.components.seconds)),
            to: environment.defaults
        )
        do {
            try await environment.relauncher.relaunch(setup.app.url)
        } catch {
            log.error("relance : \(error.localizedDescription, privacy: .public)")
            setFailed(error, during: .installation)
        }
    }

    /// L'échange tourne hors de l'acteur principal : le contrôle final relit tout le bundle.
    @concurrent
    nonisolated private static func install(_ prepared: PreparedUpdate, with installer: any UpdateInstaller) async throws -> InstalledUpdate {
        try installer.install(prepared)
    }

    private func finishInstallation(_ installed: InstalledUpdate) {
        prepared = nil
        preparedAt = nil
        preferences.preparation = nil
        preferences.clearQuitInstallFailure()
        if let previous = installed.previousBundle {
            RelaunchMarker.setPreviousBundle(previous, in: environment.defaults)
        }
    }

    /// Installation à la fermeture : l'échange se fait pendant que l'app se termine, sans relance.
    /// L'ancien bundle sera supprimé au prochain lancement, par le gate.
    private func applicationWillTerminate() {
        // Une vérification qui vient d'échouer n'empêche pas d'installer ce qui est prêt.
        guard installsOnQuit, phase != .installing, installedUpdate == nil, let prepared, case .success(let setup) = setup else {
            return
        }
        do {
            finishInstallation(try setup.installer.install(prepared))
            log.notice("installée à la fermeture : \(prepared.version, privacy: .public)")
        } catch {
            // Ni en silence, ni à chaque fermeture : la préparation est abandonnée, et l'échec sera
            // montré au prochain lancement, quand le serveur proposera de nouveau cette version.
            log.error("installation à la fermeture : \(error.localizedDescription, privacy: .public)")
            discardPreparation(setup)
            preferences.recordQuitInstallFailure(of: prepared.version, message: error.localizedDescription)
        }
    }

    private func observeTermination() {
        let center = environment.notificationCenter
        let token = center.addObserver(forName: NSApplication.willTerminateNotification, object: nil, queue: nil) { [weak self] _ in
            // AppKit la publie sur le fil principal, et la fermeture n'attend pas : l'échange se fait
            // avant de rendre la main.
            guard Thread.isMainThread else { return }
            MainActor.assumeIsolated {
                self?.applicationWillTerminate()
            }
        }
        terminationObservation = NotificationObservation(center: center, token: token)
    }

    // MARK: - Préparation gardée d'un lancement à l'autre

    /// Reprend une préparation d'un lancement précédent si elle est plus récente que l'app, récente,
    /// et que son bundle existe encore ; le contrôle final de l'installation la revérifiera. Sinon,
    /// l'archive reste dans Caches et resservira.
    private func restorePreparation(_ setup: Setup) {
        var keptArchive: URL?
        defer { downloader.removeArchives(keeping: keptArchive) }
        guard let stored = preferences.preparation else { return }
        guard stored.prepared.version > setup.app.version else {
            // Déjà installée, ou dépassée : son dossier de travail ne sert plus.
            preferences.preparation = nil
            setup.installer.discard(stored.prepared)
            return
        }
        keptArchive = downloader.archiveURL(for: stored.prepared.version)
        let isRecent = environment.now().timeIntervalSince(stored.preparedAt) <= Self.preparationLifetime
        guard isRecent, FileManager.default.fileExists(atPath: stored.prepared.bundleURL.path(percentEncoded: false)) else {
            preferences.preparation = nil
            setup.installer.discard(stored.prepared)
            return
        }
        prepared = stored.prepared
        preparedAt = stored.preparedAt
        availableUpdate = stored.update
        phase = .ready
    }

    /// Abandonne la préparation : son dossier de travail, et l'archive de Caches sauf `keepingArchive`.
    private func discardPreparation(_ setup: Setup, keepingArchive: Bool = false) {
        guard let prepared else { return }
        self.prepared = nil
        preparedAt = nil
        preferences.preparation = nil
        setup.installer.discard(prepared)
        downloader.removeArchives(keeping: keepingArchive ? downloader.archiveURL(for: prepared.version) : nil)
    }

    // MARK: - Opérations

    /// Chaque opération attend la fin de la précédente : jamais deux vérifications, téléchargements
    /// ou installations à la fois.
    @discardableResult
    private func enqueue(_ operation: @escaping @MainActor () async -> Void) -> Task<Void, Never> {
        let previous = lastOperation
        let task = Task { @MainActor in
            await previous?.value
            await operation()
        }
        lastOperation = task
        return task
    }
}

/// Retire l'observation de la fermeture avec l'updater.
private final class NotificationObservation {
    private let center: NotificationCenter
    private let token: any NSObjectProtocol

    init(center: NotificationCenter, token: any NSObjectProtocol) {
        self.center = center
        self.token = token
    }

    deinit {
        center.removeObserver(token)
    }
}
