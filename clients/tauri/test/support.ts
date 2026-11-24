/**
 * Doubles des tests : un rangement en mémoire, et un faux côté Rust qui répond aux commandes des
 * plugins (`mockIPC`) en notant chaque appel.
 */
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import type { InvokeArgs } from "@tauri-apps/api/core";
import { mockIPC } from "@tauri-apps/api/mocks";
import type { DownloadEvent } from "@tauri-apps/plugin-updater";

import type { SettingsStorage } from "../src/settings";

/** Un jeu d'essai commun à Rust, Swift et cette aide (`fixtures/` à la racine du dépôt), lu en place. */
export function fixture<T>(name: string): T {
  const path = resolve(dirname(fileURLToPath(import.meta.url)), "../../../fixtures", name);
  return JSON.parse(readFileSync(path, "utf8")) as T;
}

export class MemoryStorage implements SettingsStorage {
  readonly values = new Map<string, unknown>();
  saves = 0;

  get<T>(key: string): Promise<T | undefined> {
    return Promise.resolve(this.values.get(key) as T | undefined);
  }

  set(key: string, value: unknown): Promise<void> {
    this.values.set(key, value);
    return Promise.resolve();
  }

  delete(key: string): Promise<boolean> {
    return Promise.resolve(this.values.delete(key));
  }

  save(): Promise<void> {
    this.saves += 1;
    return Promise.resolve();
  }
}

/** Réponse de `plugin:updater|check` quand une version est proposée. */
export interface FakeRelease {
  version: string;
  currentVersion?: string;
  body?: string;
  date?: string;
  rawJson?: Record<string, unknown>;
}

export interface FakeBackend {
  /** Commandes reçues, dans l'ordre, avec leurs arguments. */
  calls: { cmd: string; args: InvokeArgs | undefined }[];
  commands(): string[];
  /** Arguments du dernier appel de cette commande. */
  last(cmd: string): Record<string, unknown>;
}

export interface FakeBackendOptions {
  /**
   * Ce que répond la recherche : une version, ou rien (à jour). Une fonction reçoit le canal
   * demandé (`X-AppsMarket-Channel`), et peut faire attendre sa réponse.
   */
  release?:
    | FakeRelease
    | null
    | ((channel: string | undefined) => FakeRelease | null | Promise<FakeRelease | null>);
  /** Événements envoyés pendant `download`. */
  downloadEvents?: DownloadEvent[];
  /** Erreur (texte, comme le plugin) renvoyée par une commande. */
  failures?: Record<string, string>;
  /** Ce qu'une commande attend avant de répondre, ou d'échouer : elle reste en vol d'ici là. */
  gates?: Record<string, Promise<unknown>>;
  version?: string;
  /**
   * Version du système que donne le plugin os ; `null` : le plugin n'est pas enregistré côté
   * Rust. Par défaut, `14.6.1`.
   */
  osVersion?: string | null;
}

/** Nom de ce que le plugin os injecte dans la page, et que lit sa fonction `version()`. */
const OS_INTERNALS = "__TAURI_OS_PLUGIN_INTERNALS__";

export const OS_VERSION = "14.6.1";
export const UPDATE_RID = 41;
export const BYTES_RID = 42;

export function fakeBackend(options: FakeBackendOptions = {}): FakeBackend {
  const calls: FakeBackend["calls"] = [];
  const osVersion = options.osVersion === undefined ? OS_VERSION : options.osVersion;
  if (osVersion === null) {
    Reflect.deleteProperty(window, OS_INTERNALS);
  } else {
    Reflect.set(window, OS_INTERNALS, { version: osVersion });
  }
  mockIPC(async (cmd, args) => {
    calls.push({ cmd, args });
    await options.gates?.[cmd];
    const failure = options.failures?.[cmd];
    if (failure !== undefined) {
      return Promise.reject(failure);
    }
    switch (cmd) {
      case "plugin:app|version":
        return options.version ?? "0.1.0";
      case "plugin:updater|check": {
        const headers = new Headers((args as { headers?: [string, string][] }).headers);
        const release =
          typeof options.release === "function"
            ? await options.release(headers.get("x-appsmarket-channel") ?? undefined)
            : (options.release ?? null);
        if (release === null) {
          return null;
        }
        return {
          rid: UPDATE_RID,
          currentVersion: release.currentVersion ?? "0.1.0",
          version: release.version,
          date: release.date,
          body: release.body,
          rawJson: release.rawJson ?? { version: release.version, channel: "stable" },
        };
      }
      case "plugin:updater|download": {
        const channel = (args as { onEvent: { onmessage: (event: DownloadEvent) => void } })
          .onEvent;
        for (const event of options.downloadEvents ?? []) {
          channel.onmessage(event);
        }
        return BYTES_RID;
      }
      case "plugin:updater|install":
      case "plugin:resources|close":
      case "plugin:process|restart":
        return null;
      default:
        return Promise.reject(`commande inattendue : ${cmd}`);
    }
  });
  return {
    calls,
    commands: () => calls.map((call) => call.cmd),
    last(cmd) {
      const call = calls.findLast((candidate) => candidate.cmd === cmd);
      if (call === undefined) {
        throw new Error(`${cmd} n'a pas été appelée`);
      }
      return (call.args ?? {}) as Record<string, unknown>;
    },
  };
}
