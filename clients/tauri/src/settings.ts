/**
 * Réglages de mise à jour et identifiant d'installation, rangés dans les données de l'app :
 * le fichier `appsmarket.json` du plugin store, dans le dossier de données de l'app
 * (`~/Library/Application Support/<identifiant>` sur macOS, `%APPDATA%\<identifiant>` sous
 * Windows, `~/.local/share/<identifiant>` sous Linux).
 */
import { load } from "@tauri-apps/plugin-store";

/** Canal suivi par l'app : `beta` voit aussi les versions stables. */
export type UpdateChannel = "stable" | "beta";

export interface UpdaterSettings {
  /** Canal suivi, `stable` par défaut. */
  channel: UpdateChannel;
  /** Statistiques anonymes : activées par défaut, l'utilisateur peut les couper. */
  shareStatistics: boolean;
}

/**
 * Ce que l'aide attend du rangement. Le `Store` du plugin store convient tel quel ; un double en
 * mémoire sert aux tests.
 */
export interface SettingsStorage {
  get<T>(key: string): Promise<T | undefined>;
  set(key: string, value: unknown): Promise<void>;
  delete(key: string): Promise<boolean>;
  save(): Promise<void>;
}

/** Fichier du plugin store, relatif au dossier de données de l'app. */
export const SETTINGS_FILE = "appsmarket.json";

const KEYS = {
  channel: "channel",
  shareStatistics: "shareStatistics",
  installId: "installId",
} as const;

const UUID_V4 = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

let sharedStorage: Promise<SettingsStorage> | undefined;

/** Le fichier `appsmarket.json`, ouvert une seule fois. */
export function defaultStorage(): Promise<SettingsStorage> {
  sharedStorage ??= load(SETTINGS_FILE, { autoSave: false, defaults: {} }).catch(
    (error: unknown) => {
      // Un échec (plugin store absent, par exemple) ne doit pas rester en cache.
      sharedStorage = undefined;
      throw error;
    },
  );
  return sharedStorage;
}

// Les modifications passent l'une après l'autre : deux premières recherches simultanées ne
// doivent pas créer deux identifiants.
let queue: Promise<unknown> = Promise.resolve();

function serialized<T>(task: () => Promise<T>): Promise<T> {
  const result = queue.then(task, task);
  queue = result.catch(() => undefined);
  return result;
}

/** Lit les réglages ; une valeur absente ou illisible prend sa valeur par défaut. */
export async function loadSettings(storage?: SettingsStorage): Promise<UpdaterSettings> {
  const store = storage ?? (await defaultStorage());
  const channel = await store.get<unknown>(KEYS.channel);
  const shareStatistics = await store.get<unknown>(KEYS.shareStatistics);
  return {
    channel: channel === "beta" ? "beta" : "stable",
    shareStatistics: typeof shareStatistics === "boolean" ? shareStatistics : true,
  };
}

/**
 * Enregistre des réglages et renvoie l'ensemble à jour. Couper les statistiques efface
 * l'identifiant d'installation : si l'utilisateur les réactive, il en reçoit un nouveau, sans lien
 * avec l'ancien.
 */
export function saveSettings(
  changes: Partial<UpdaterSettings>,
  storage?: SettingsStorage,
): Promise<UpdaterSettings> {
  return serialized(async () => {
    const store = storage ?? (await defaultStorage());
    if (changes.channel !== undefined) {
      await store.set(KEYS.channel, changes.channel === "beta" ? "beta" : "stable");
    }
    if (changes.shareStatistics !== undefined) {
      await store.set(KEYS.shareStatistics, changes.shareStatistics);
      if (!changes.shareStatistics) {
        await store.delete(KEYS.installId);
      }
    }
    await store.save();
    return loadSettings(store);
  });
}

/**
 * Identifiant aléatoire de l'installation (UUID v4), créé à la première demande puis gardé.
 * Il n'est jamais dérivé du matériel, et seul l'appelant décide de l'envoyer.
 */
export function installId(storage?: SettingsStorage): Promise<string> {
  return serialized(async () => {
    const store = storage ?? (await defaultStorage());
    const stored = await store.get<unknown>(KEYS.installId);
    if (typeof stored === "string" && UUID_V4.test(stored)) {
      return stored;
    }
    const id = randomUuidV4();
    await store.set(KEYS.installId, id);
    await store.save();
    return id;
  });
}

/** UUID v4 en minuscules, tiré de `crypto.getRandomValues`. */
export function randomUuidV4(): string {
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  bytes[6] = ((bytes[6] ?? 0) & 0x0f) | 0x40;
  bytes[8] = ((bytes[8] ?? 0) & 0x3f) | 0x80;
  const hex = Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
  return [
    hex.slice(0, 8),
    hex.slice(8, 12),
    hex.slice(12, 16),
    hex.slice(16, 20),
    hex.slice(20),
  ].join("-");
}
