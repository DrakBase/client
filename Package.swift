// swift-tools-version: 6.2
// Manifeste d'installation du paquet DrakBase par l'adresse du dépôt public des clients, par HTTPS
// (scripts/publish-client.sh le publie dans DrakBase/client) :
//
//   .package(url: "https://github.com/DrakBase/client.git", from: "0.6.0")
//   .product(name: "DrakBase", package: "client")
//   import DrakBase
//
// SwiftPM n'accepte un paquet distant qu'avec son manifeste à la racine du dépôt : celui-ci expose les
// sources de clients/swift/DrakBase, sans les déplacer. Le paquet de développement, avec ses
// tests et ses réglages, reste là-bas (swift test --package-path clients/swift/DrakBase).
import PackageDescription

let sources = "clients/swift/DrakBase/Sources"

/// Les mêmes règles que le paquet de développement : Swift 6, avertissements traités comme des erreurs.
let swiftSettings: [SwiftSetting] = [
    .treatAllWarnings(as: .error),
]

let package = Package(
    name: "DrakBase",
    platforms: [.macOS(.v14)],
    products: [
        .library(
            name: "DrakBase",
            type: .static,
            targets: ["DrakBase", "AppsMarketCore", "AppsMarketCrypto", "AppsMarketUI"]
        ),
    ],
    targets: [
        .target(name: "AppsMarketCrypto", path: "\(sources)/AppsMarketCrypto", swiftSettings: swiftSettings),
        .target(
            name: "AppsMarketCore",
            dependencies: ["AppsMarketCrypto"],
            path: "\(sources)/AppsMarketCore",
            swiftSettings: swiftSettings
        ),
        .target(
            name: "AppsMarketUI",
            dependencies: ["AppsMarketCore"],
            path: "\(sources)/AppsMarketUI",
            swiftSettings: swiftSettings
        ),
        .target(
            name: "DrakBase",
            dependencies: ["AppsMarketCore", "AppsMarketCrypto", "AppsMarketUI"],
            path: "\(sources)/DrakBase",
            swiftSettings: swiftSettings
        ),
    ],
    swiftLanguageModes: [.v6]
)
