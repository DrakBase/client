# DrakBase, le paquet Swift

Paquet Swift qui met à jour les apps macOS (Translate, Kiosque, Pli) auprès du serveur DrakBase - Apps Market. La conception est dans [docs/PLAN.md](../../../docs/PLAN.md), section « Paquet Swift », et le contrat avec le serveur dans [docs/PROTOCOLE.md](../../../docs/PROTOCOLE.md).

## État

Jalons A3 (cryptographie, versions, requête de vérification) et B4 (`Updater`, installation locale, gate de relance, interface). Kiosque intègre le paquet depuis le 2026-10-27, première moitié du jalon C3 : sa version 1.1.0 est publiée, signée par le certificat de signature de code. Translate le branche aussi dans son code (gate, updater, réglages). Le service XPC des apps sandboxées viendra au jalon C2, avec l'intégration à Pli.

| Module | Contenu |
|---|---|
| `AppsMarketCrypto` | BLAKE2b-512 en flux (RFC 7693) ; clés et signatures minisign au format Tauri ; vérification dans l'ordre du protocole ; `MinisignKeyring`, clé principale et clés de secours |
| `AppsMarketCore` | `SemanticVersion`, `UpdateResponse`, `UpdateFeed` ; `UpdaterConfiguration` (`Info.plist`) ; vérification, téléchargement dans Caches ; `UpdateInstaller` et `LocalInstaller` ; `RelaunchGate` ; `Updater` |
| `AppsMarketUI` | `UpdateSettingsSection` et ses contrôles séparés, `UpdatesCommands`, fenêtre `UpdatePrompt`, en français |

## Réglages

- Outils Swift 6.2, macOS 14 au minimum, mode Swift 6, avertissements traités comme des erreurs, aucune dépendance.
- Un seul produit, `DrakBase`, lié statiquement, et un seul module à importer, `DrakBase`, qui réexporte `AppsMarketCore`, `AppsMarketUI` et `AppsMarketCrypto` : ces modules internes gardent leur nom d'avant la 0.3. Aucune ressource SwiftPM : les textes sont dans le code.
- Aucune isolation par défaut : seuls `Updater`, son délégué, sa présentation et l'interface sont sur le MainActor. Le reste est non isolé et `Sendable`, et le travail lourd (hachage, extraction, contrôle de signature, échange) tourne dans des fonctions `@concurrent`. Le module de test `AppsMarketMainActorTests` est compilé comme Translate et Pli (isolation MainActor par défaut, concurrence « accessible ») pour le vérifier.

## Configuration

Dans l'`Info.plist` signé de l'app (`docs/PROTOCOLE.md`, « Intégration ») :

| Clé | Contenu |
|---|---|
| `DrakBaseFeedURL` | `https://updates.drakbase.com/api/v1/update/translate` |
| `DrakBasePublicKeys` | clé principale, puis clé de secours, au format Tauri |
| `DrakBaseTrustedRequirements` | facultatif : exigences de signature de code acceptées en plus de l'exigence désignée de l'app installée |
| `DrakBaseInstallerService` | facultatif : service XPC d'installation d'une app sandboxée (C2) ; d'ici là, l'installation est indisponible |

Les anciennes clés `AppsMarket…` (avant la 0.3) restent lues quand la nouvelle manque ; `drakbase init` les renomme.

Sans configuration complète, l'updater reste `unavailable` et le dit dans l'interface.

## Installation

Le paquet s'installe depuis le dépôt public des clients de DrakBase, par HTTPS : le manifeste de la racine du dépôt (`Package.swift`) expose le produit `DrakBase`. Les versions suivent les étiquettes du dépôt (`v0.6.0`…). Jusqu'à la 0.2, le produit s'appelait `AppsMarketKit`.

```swift
dependencies: [
    .package(url: "https://github.com/DrakBase/client.git", from: "0.6.0"),
],
targets: [
    .executableTarget(name: "MonApp", dependencies: [
        .product(name: "DrakBase", package: "client"),
    ]),
]
```

- Dans Xcode : File › Add Package Dependencies…, avec la même adresse.
- `drakbase init <app>`, à la racine du projet :
  - crée les clés de l'app s'il le faut, et les sauvegarde dans KeePassXC ;
  - écrit `DrakBaseFeedURL` et `DrakBasePublicKeys` dans l'Info.plist, et renomme ces clés si elles portent encore leur ancien nom ;
  - déclare l'app et sa clé sur le serveur, par une page pré-remplie de l'administration ;
  - finit par les contrôles de `drakbase doctor` : la configuration, la dépendance, le code ci-dessous, l'app sur le serveur, la sauvegarde des clés et le certificat de signature de code.
- Pour essayer une modification du paquet avant de l'étiqueter, une app voisine peut le prendre par son chemin : `.package(path: "../DrakBase - Apps Market/clients/swift/DrakBase")`.

## Intégration

```swift
import DrakBase
import SwiftUI

@main
@MainActor
enum Main {
    static func main() {
        // Avant toute autre chose : la nouvelle instance attend la fermeture de l'ancienne.
        RelaunchGate.waitForPreviousInstance()
        TranslateApp.main()
    }
}

struct TranslateApp: App {
    @State private var updater: Updater = {
        let updater = Updater()  // Updater(allowsAnonymousStatistics: false) pour Pli
        updater.presenter = UpdatePrompt(updater: updater)
        updater.start()
        return updater
    }()

    var body: some Scene {
        WindowGroup { ContentView() }
            .commands { UpdatesCommands(updater: updater) }
        Settings {
            Form { UpdateSettingsSection(updater: updater) }.formStyle(.grouped)
        }
    }
}
```

- `@MainActor` sur `Main` : `App.main()` est isolé sur le MainActor, `Main` l'est donc aussi. Une app dont le module est isolé par défaut (Translate, Pli) peut s'en passer.
- `updater.delegate` reçoit `updaterShouldRelaunch(_:)` juste avant l'échange, pour enregistrer un brouillon ou refuser. Après `true`, l'app doit accepter de se fermer.
- Une interface à soi pilote l'updater par `checkNow()`, `installAndRelaunch()`, `skip(_:)` et `retry()`. Après un échec (`phase == .failed`), `retry()` refait ce qui a échoué : la vérification si c'est elle, ou si la version a été retirée ; sinon l'installation, ou la seule relance. C'est ce que fait « Réessayer » dans `UpdatePrompt`.
- Les réglages (`automaticChecks`, `checkInterval`, `installsOnQuit`, `channel`, `sharesAnonymousStatistics`), l'identifiant d'installation, la version ignorée et la préparation en cours sont rangés dans les préférences de l'app, sous des clés `AppsMarketKit.*` : elles gardent l'ancien nom du paquet, pour que les installations existantes ne perdent pas leurs réglages.

## Déroulé

1. Vérification au lancement, puis à intervalle (un jour par défaut, entre une heure et trente jours), ou à la demande (`checkNow()`). L'identifiant d'installation part en en-tête si les statistiques sont acceptées. La réponse est lue sur 1 Mio et 60 s au plus ; une version qui n'est pas plus récente, qui exige un macOS plus récent, ou une bêta proposée au canal stable, compte comme « à jour ». Si le canal change pendant la requête, la réponse valait pour l'ancien : la vérification est refaite avec le nouveau. Une vérification qui échoue ne fait pas oublier une version déjà trouvée : automatique, elle se tait ; demandée, elle montre son erreur, et « Réessayer » vérifie de nouveau, sans rien installer.
2. Téléchargement dans `Caches/<identifiant de l'app>/AppsMarketKit`, 60 s au plus sans octet reçu et 4 h en tout, taille contrôlée pendant la réception, empreinte SHA-256 à la fin. Une version retirée entre-temps (`410`) est oubliée : « Réessayer » relance alors une vérification, au lieu de retélécharger la même adresse.
3. Vérification minisign avec la clé embarquée dont l'identifiant est celui de la signature, et version signée égale à la version annoncée.
4. Préparation par `LocalInstaller` : copie dans un dossier de travail sur le volume de l'app (`itemReplacementDirectory`), nouvelle vérification, extraction par `/usr/bin/tar`, un seul `.app` ; même identifiant, version égale à l'annoncée et plus récente que celle qui tourne comme que celle du disque, `LSMinimumSystemVersion` compatible, lu comme le serveur lit `min_os_version` (`docs/PROTOCOLE.md`, « Versions d'OS ») et refusé s'il est illisible ; signature de code conforme à l'exigence désignée de l'app installée ou à `DrakBaseTrustedRequirements`, refus d'une app installée ad hoc ; retrait de la quarantaine. La préparation survit à une relance de l'app ; au-delà d'un jour, elle est refaite depuis l'archive de Caches, car macOS vide les dossiers temporaires.
5. App transloquée ou dossier non modifiable : `unavailable`, avec le message « déplacez l'app dans Applications ».
6. Installation : contrôle final, `renamex_np(RENAME_SWAP)`, avec un repli en deux renommages sur un volume qui refuse l'échange atomique, puis nouveau contrôle du bundle à sa place définitive. S'il a été remplacé entre-temps, l'ancienne version reprend sa place, par l'un ou l'autre chemin. Si elle ne peut pas la reprendre, elle reste dans le dossier de travail (dans `previous/`, ou à défaut là où l'échange l'a laissée, à la place du bundle préparé), et l'erreur dit où. Le paquet ne supprime plus ce dossier : `discard` le garde tant que `previous/` n'est pas vide, ou que la place du bundle préparé contient autre chose que la version préparée. C'est un dossier temporaire, que macOS vide au redémarrage, ou après quelques jours sans usage : l'app est à remettre en place à la main d'ici là. Ensuite `LSRegisterURL`, marque de relance, ouverture de la nouvelle instance, puis fermeture dès que `NSRunningApplication` la montre. Sans nouvelle instance au bout de 10 s, l'app reste ouverte et le dit ; « Réessayer » ne refait alors que la relance, et la vérification automatique s'arrête, puisqu'il n'y a plus rien à vérifier. Avec `installsOnQuit`, l'échange se fait à la fermeture, sans relance ; s'il échoue, il n'est pas retenté en silence : l'échec est montré au lancement suivant.
7. Gate : la nouvelle instance attend la fin de l'ancienne (`kill(pid, 0)`, `EPERM` voulant dire « vivante »), abandonne au bout de 20 s, sinon supprime l'ancien bundle.

## Tests

```bash
swift test --package-path clients/swift/DrakBase
```

- Les tests rejouent en place les jeux d'essai communs à Rust et Swift (`fixtures/` à la racine du dépôt) et doivent y donner exactement les résultats attendus.
- Vérification et téléchargement contre un faux serveur (`URLProtocol`), installation sur de fausses apps dans un dossier temporaire, signées par une clé minisign jetable, échange, gate, et un essai de bout en bout par le vrai code d'installation.
- Les fausses apps des tests ne sont signées par aucun certificat, pour que les tests ne dépendent ni d'un Mac ni de son trousseau : le contrôle de signature de code y est remplacé, et là seulement, par une politique simulée injectée par `@testable import`. La vraie politique est essayée à part, sur des apps non signées, signées ad hoc et signées par Apple.
- Avec le certificat de signature de code, créé le 2026-10-27, Kiosque 1.1.0 est publiée, et l'app de son archive satisfait l'exigence désignée. Reste à voir une vraie mise à jour installée par le paquet depuis une app signée par ce certificat : c'est la phase 2 de M0, puis Kiosque 1.1.1.
