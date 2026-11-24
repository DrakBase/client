// swift-tools-version: 6.2
import PackageDescription

/// Règles communes : Swift 6 (concurrence stricte) et avertissements traités comme des erreurs.
/// Pas d'isolation par défaut : les types restent non isolés et `Sendable`, utilisables depuis le
/// code MainActor de Translate et Pli comme depuis une tâche de fond. Seuls `Updater` et
/// l'interface sont explicitement sur le MainActor.
let swiftSettings: [SwiftSetting] = [
    .treatAllWarnings(as: .error),
]

let package = Package(
    name: "DrakBase",
    // macOS 14 pour Kiosque.
    platforms: [.macOS(.v14)],
    products: [
        // Un seul produit, DrakBase, et un seul module à importer, `DrakBase`, qui réexporte les
        // autres. Liaison statique : avec un certificat auto-signé, sans Team ID, le runtime
        // renforcé refuserait une bibliothèque dynamique. Aucune ressource SwiftPM, car Kiosque
        // n'embarque que le binaire.
        .library(
            name: "DrakBase",
            type: .static,
            targets: ["DrakBase", "AppsMarketCore", "AppsMarketCrypto", "AppsMarketUI"]
        ),
    ],
    targets: [
        // BLAKE2b-512 en flux et vérification des signatures minisign au format Tauri.
        .target(name: "AppsMarketCrypto", swiftSettings: swiftSettings),
        // Versions, vérification, téléchargement, installation, gate de relance et `Updater`.
        .target(name: "AppsMarketCore", dependencies: ["AppsMarketCrypto"], swiftSettings: swiftSettings),
        // Réglages, menu et fenêtre de mise à jour, en français.
        .target(name: "AppsMarketUI", dependencies: ["AppsMarketCore"], swiftSettings: swiftSettings),
        // Ce que l'app importe : les trois modules précédents, réexportés.
        .target(
            name: "DrakBase",
            dependencies: ["AppsMarketCore", "AppsMarketCrypto", "AppsMarketUI"],
            swiftSettings: swiftSettings
        ),
        // Jeux d'essai communs à Rust et Swift (`fixtures/`), fausses apps, faux serveur et clé
        // d'essai : jamais livré dans l'app.
        .target(
            name: "AppsMarketTestSupport",
            dependencies: ["AppsMarketCrypto"],
            path: "Tests/AppsMarketTestSupport",
            swiftSettings: swiftSettings
        ),
        .testTarget(
            name: "AppsMarketCryptoTests",
            dependencies: ["AppsMarketCrypto", "AppsMarketTestSupport"],
            swiftSettings: swiftSettings
        ),
        .testTarget(
            name: "AppsMarketCoreTests",
            dependencies: ["AppsMarketCore", "AppsMarketCrypto", "AppsMarketTestSupport"],
            swiftSettings: swiftSettings
        ),
        .testTarget(
            name: "AppsMarketUITests",
            dependencies: ["AppsMarketUI", "AppsMarketCore"],
            swiftSettings: swiftSettings
        ),
        // Compilé comme Translate et Pli : isolation MainActor par défaut et concurrence
        // « accessible » (SWIFT_APPROACHABLE_CONCURRENCY), pour prouver que l'API publique s'y
        // utilise sans avertissement, par le seul `import DrakBase` d'une app.
        .testTarget(
            name: "AppsMarketMainActorTests",
            dependencies: ["DrakBase", "AppsMarketTestSupport"],
            swiftSettings: swiftSettings + [
                .defaultIsolation(MainActor.self),
                .enableUpcomingFeature("NonisolatedNonsendingByDefault"),
                .enableUpcomingFeature("InferIsolatedConformances"),
                .enableUpcomingFeature("MemberImportVisibility"),
            ]
        ),
    ],
    swiftLanguageModes: [.v6]
)
