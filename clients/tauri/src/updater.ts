/**
 * Recherche et installation d'une mise à jour auprès d'un serveur DrakBase - Apps Market, avec le plugin
 * officiel `tauri-plugin-updater` (docs/PROTOCOLE.md, section « Intégration »).
 *
 * L'adresse du serveur, la clé publique et `requireSignedVersion` sont dans `tauri.conf.json` ;
 * cette aide n'ajoute que ce que le JavaScript peut ajouter : des en-têtes.
 */
import { version as systemVersion } from "@tauri-apps/plugin-os";
import { relaunch } from "@tauri-apps/plugin-process";
import { check, type DownloadEvent, type Update } from "@tauri-apps/plugin-updater";

import { osVersionAtLeast, parseOsVersion } from "./os-version";
import {
  defaultStorage,
  installId,
  loadSettings,
  type SettingsStorage,
  type UpdateChannel,
} from "./settings";

/** En-têtes lus par le serveur (docs/PROTOCOLE.md, « Vérification »). */
const CHANNEL_HEADER = "X-AppsMarket-Channel";
const INSTALL_ID_HEADER = "X-AppsMarket-Install-Id";
const OS_VERSION_HEADER = "X-AppsMarket-OS-Version";

/**
 * Délai par défaut d'une recherche, en millisecondes : 60 s, comme dans le paquet Swift. Sans
 * délai, le plugin attendrait sans fin un serveur qui ne répond pas.
 */
const DEFAULT_CHECK_TIMEOUT = 60_000;

/**
 * Délai par défaut d'un téléchargement, en millisecondes : 4 h au total, comme dans le paquet
 * Swift. De quoi passer une grosse archive sur une connexion lente, sans laisser un serveur qui
 * traîne retenir l'app indéfiniment.
 */
const DEFAULT_DOWNLOAD_TIMEOUT = 4 * 3_600_000;

export interface CheckForUpdateOptions {
  /** Canal de cette recherche ; par défaut, celui des réglages. */
  channel?: UpdateChannel | undefined;
  /** Envoyer l'identifiant d'installation ; par défaut, le réglage des statistiques. */
  shareStatistics?: boolean | undefined;
  /**
   * Version de l'OS, pour que le serveur écarte les versions qui exigent un OS plus récent. Par
   * défaut, celle que donne le plugin os ; une chaîne vide n'envoie rien.
   */
  osVersion?: string | undefined;
  /**
   * Délai de la recherche, en millisecondes ; 60 000 par défaut. Passé ce délai, le plugin
   * abandonne la requête et la recherche échoue.
   */
  timeout?: number | undefined;
  /** Rangement des réglages ; par défaut, `appsmarket.json` dans les données de l'app. */
  storage?: SettingsStorage | undefined;
}

/**
 * Demande au serveur s'il existe une version plus récente. Renvoie la mise à jour du plugin, ou
 * `null` si l'app est à jour.
 *
 * Le canal part dans `X-AppsMarket-Channel`, la version du système dans
 * `X-AppsMarket-OS-Version`, et l'identifiant d'installation dans `X-AppsMarket-Install-Id`,
 * seulement si l'utilisateur garde les statistiques.
 *
 * Comme le paquet Swift, l'aide ne se fie pas au seul serveur :
 * - une réponse dont le `channel` n'est ni `stable` ni `beta` est refusée, par une erreur ;
 * - une bêta reçue alors que l'app suit le canal stable, ou une version qui exige un système plus
 *   récent que celui de la machine (`min_os_version`), est traitée comme « à jour » : c'est une
 *   incohérence du serveur, pas une erreur à montrer.
 */
export async function checkForUpdate(options: CheckForUpdateOptions = {}): Promise<Update | null> {
  const storage = options.storage ?? (await defaultStorage());
  const settings = await loadSettings(storage);
  const channel = options.channel ?? settings.channel;
  const headers: Record<string, string> = { [CHANNEL_HEADER]: channel };
  if (options.shareStatistics ?? settings.shareStatistics) {
    headers[INSTALL_ID_HEADER] = await installId(storage);
  }
  const osVersion = options.osVersion ?? detectedOsVersion();
  if (osVersion) {
    headers[OS_VERSION_HEADER] = osVersion;
  }
  const update = await check({ headers, timeout: options.timeout ?? DEFAULT_CHECK_TIMEOUT });
  if (update === null) {
    return null;
  }
  const announced = update.rawJson["channel"];
  if (announced !== "stable" && announced !== "beta") {
    await update.close();
    throw new Error(
      announced === undefined
        ? "réponse du serveur illisible : champ « channel » absent"
        : `réponse du serveur illisible : canal ${JSON.stringify(announced)} inconnu`,
    );
  }
  if ((channel === "stable" && announced === "beta") || requiresNewerSystem(update, osVersion)) {
    await update.close();
    return null;
  }
  return update;
}

/**
 * Vrai si la version proposée exige un système plus récent que `osVersion`. Sans version du
 * système, ou si l'une des deux est illisible, rien n'est écarté, comme côté serveur.
 */
function requiresNewerSystem(update: Pick<Update, "rawJson">, osVersion: string | undefined): boolean {
  const minimum = update.rawJson["min_os_version"];
  if (!osVersion || typeof minimum !== "string") {
    return false;
  }
  const current = parseOsVersion(osVersion);
  const required = parseOsVersion(minimum);
  return current !== null && required !== null && !osVersionAtLeast(current, required);
}

/**
 * Version du système, telle que la donne le plugin os (`14.6.1` sur macOS). Rien si le plugin
 * n'est pas enregistré côté Rust : la recherche part alors sans cet en-tête, et le serveur ne peut
 * pas écarter une version trop récente pour la machine.
 */
function detectedOsVersion(): string | undefined {
  try {
    const version: unknown = systemVersion();
    return typeof version === "string" && version !== "" ? version : undefined;
  } catch {
    return undefined;
  }
}

/**
 * Canal de la version proposée, tel que l'annonce le serveur (champ `channel`). Une mise à jour
 * rendue par `checkForUpdate` porte toujours `stable` ou `beta` ; pour une autre, toute autre
 * valeur vaut `stable`.
 */
export function updateChannel(update: Pick<Update, "rawJson">): UpdateChannel {
  return update.rawJson["channel"] === "beta" ? "beta" : "stable";
}

/** Avancement du téléchargement ; `total` est inconnu tant que le serveur ne l'a pas donné. */
export interface DownloadProgress {
  downloaded: number;
  total: number | null;
}

export interface InstallUpdateOptions {
  onProgress?: ((progress: DownloadProgress) => void) | undefined;
  /**
   * Délai du téléchargement, en millisecondes, du premier au dernier octet ; 4 h par défaut. Passé
   * ce délai, le plugin abandonne le téléchargement et l'installation échoue.
   */
  downloadTimeout?: number | undefined;
  /**
   * Appelé quand l'archive est téléchargée et sa signature vérifiée, juste avant l'installation :
   * c'est le dernier moment pour enregistrer un travail en cours, car sous Windows l'installeur
   * ferme l'app. Une erreur levée ici annule l'installation.
   */
  beforeInstall?: (() => void | Promise<void>) | undefined;
}

/**
 * Télécharge la mise à jour, la fait vérifier par le plugin (signature minisign, puis version
 * signée avec `requireSignedVersion`), l'installe, puis relance l'app.
 *
 * Sur macOS et Linux, le plugin remplace l'app puis rend la main : `relaunch()` démarre la
 * nouvelle version. Sous Windows, l'installeur NSIS ferme l'app et la relance lui-même, si bien
 * que cette fonction ne revient pas.
 */
export async function installUpdate(
  update: Update,
  options: InstallUpdateOptions = {},
): Promise<void> {
  let downloaded = 0;
  let total: number | null = null;
  const onEvent = (event: DownloadEvent) => {
    switch (event.event) {
      case "Started":
        downloaded = 0;
        total = event.data.contentLength ?? null;
        break;
      case "Progress":
        downloaded += event.data.chunkLength;
        break;
      case "Finished":
        break;
    }
    options.onProgress?.({ downloaded, total });
  };
  // Sans en-têtes de l'app : l'identifiant d'installation ne sert qu'à la recherche. Avec son
  // propre délai : le plugin n'applique pas ici celui de la recherche, et attendrait sinon sans fin
  // un serveur qui cesse d'envoyer.
  await update.download(onEvent, {
    headers: {},
    timeout: options.downloadTimeout ?? DEFAULT_DOWNLOAD_TIMEOUT,
  });
  await options.beforeInstall?.();
  await update.install();
  await relaunch();
}

/** Texte d'une erreur du plugin, sans les accents graves qui entourent certains messages. */
function errorText(error: unknown): string {
  const text = error instanceof Error ? error.message : String(error);
  return text.replace(/^`(.*)`$/s, "$1");
}

/**
 * Vrai si le téléchargement a reçu `410 Gone` : l'éditeur a retiré la version entre la recherche
 * et le téléchargement. Réessayer ne sert à rien, il faut chercher à nouveau.
 */
export function isWithdrawnUpdateError(error: unknown): boolean {
  return /status: 410\b/.test(errorText(error));
}

/**
 * Message en français pour une erreur du plugin, dont les messages sont en anglais. Le texte
 * d'origine reste à la fin, entre parenthèses, pour le diagnostic.
 */
export function describeUpdateError(error: unknown): string {
  const detail = errorText(error);
  if (isWithdrawnUpdateError(detail)) {
    return `cette version a été retirée par l'éditeur ; recherche à nouveau (${detail})`;
  }
  if (/status: 404\b/.test(detail)) {
    return `le fichier de la mise à jour est introuvable sur le serveur (${detail})`;
  }
  if (/signed for version|does not specify the version/.test(detail)) {
    return `mise à jour refusée : la version signée n'est pas celle annoncée (${detail})`;
  }
  if (/signature|key id|algorithm/i.test(detail)) {
    return `mise à jour refusée : signature invalide (${detail})`;
  }
  if (/valid release JSON/.test(detail)) {
    return `le serveur de mises à jour n'a pas donné de réponse valable (${detail})`;
  }
  if (/error decoding response body|request or response body error/.test(detail)) {
    return `réponse du serveur interrompue, trop longue à venir ou illisible (${detail})`;
  }
  if (/error sending request|connect|timed out|dns/i.test(detail)) {
    return `serveur de mises à jour injoignable (${detail})`;
  }
  return detail;
}
