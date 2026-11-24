import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { installId, loadSettings, randomUuidV4, saveSettings } from "../src/settings";
import { MemoryStorage } from "./support";

const UUID_V4 = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

afterEach(() => {
  clearMocks();
});

describe("randomUuidV4", () => {
  it("donne des UUID v4 en minuscules, tous différents", () => {
    const ids = new Set(Array.from({ length: 200 }, () => randomUuidV4()));
    expect(ids.size).toBe(200);
    for (const id of ids) {
      expect(id).toMatch(UUID_V4);
    }
  });
});

describe("loadSettings", () => {
  it("prend le canal stable et les statistiques actives par défaut", async () => {
    expect(await loadSettings(new MemoryStorage())).toEqual({
      channel: "stable",
      shareStatistics: true,
    });
  });

  it("remplace une valeur illisible par sa valeur par défaut", async () => {
    const storage = new MemoryStorage();
    storage.values.set("channel", "nightly");
    storage.values.set("shareStatistics", "non");
    expect(await loadSettings(storage)).toEqual({ channel: "stable", shareStatistics: true });
    storage.values.set("channel", "beta");
    storage.values.set("shareStatistics", false);
    expect(await loadSettings(storage)).toEqual({ channel: "beta", shareStatistics: false });
  });
});

describe("saveSettings", () => {
  it("enregistre les réglages sur le disque", async () => {
    const storage = new MemoryStorage();
    expect(await saveSettings({ channel: "beta" }, storage)).toEqual({
      channel: "beta",
      shareStatistics: true,
    });
    expect(storage.values.get("channel")).toBe("beta");
    expect(storage.saves).toBe(1);
  });

  it("efface l'identifiant quand l'utilisateur coupe les statistiques", async () => {
    const storage = new MemoryStorage();
    const first = await installId(storage);
    await saveSettings({ shareStatistics: false }, storage);
    expect(storage.values.has("installId")).toBe(false);
    await saveSettings({ shareStatistics: true }, storage);
    const second = await installId(storage);
    expect(second).toMatch(UUID_V4);
    expect(second).not.toBe(first);
  });
});

describe("installId", () => {
  it("crée l'identifiant une fois, puis le garde", async () => {
    const storage = new MemoryStorage();
    const id = await installId(storage);
    expect(id).toMatch(UUID_V4);
    expect(await installId(storage)).toBe(id);
    expect(storage.saves).toBe(1);
  });

  it("ne crée qu'un identifiant pour deux demandes simultanées", async () => {
    const storage = new MemoryStorage();
    const [a, b] = await Promise.all([installId(storage), installId(storage)]);
    expect(a).toBe(b);
    expect(storage.saves).toBe(1);
  });

  it("remplace un identifiant qui n'est pas un UUID v4", async () => {
    const storage = new MemoryStorage();
    storage.values.set("installId", "machine-de-camille");
    const id = await installId(storage);
    expect(id).toMatch(UUID_V4);
    expect(storage.values.get("installId")).toBe(id);
  });
});

describe("defaultStorage", () => {
  // Le fichier ouvert est gardé par le module : chaque cas repart d'un module neuf.
  beforeEach(() => {
    vi.resetModules();
  });

  it("ouvre appsmarket.json par le plugin store, sans enregistrement automatique", async () => {
    const { SETTINGS_FILE, defaultStorage } = await import("../src/settings");
    const calls: { cmd: string; args: unknown }[] = [];
    mockIPC((cmd, args) => {
      calls.push({ cmd, args });
      if (cmd === "plugin:store|load") {
        return 7;
      }
      if (cmd === "plugin:store|get") {
        return [null, false];
      }
      return null;
    });
    const storage = await defaultStorage();
    expect(await storage.get("channel")).toBeUndefined();
    expect(calls[0]).toEqual({
      cmd: "plugin:store|load",
      args: { path: SETTINGS_FILE, options: { autoSave: false, defaults: {} } },
    });
    expect(calls[1]).toEqual({ cmd: "plugin:store|get", args: { rid: 7, key: "channel" } });
    // Le fichier n'est ouvert qu'une fois.
    expect(await defaultStorage()).toBe(storage);
  });

  it("ne garde pas un échec d'ouverture", async () => {
    const { defaultStorage } = await import("../src/settings");
    let attempts = 0;
    mockIPC((cmd) => {
      if (cmd === "plugin:store|load") {
        attempts += 1;
        return attempts === 1 ? Promise.reject("plugin store absent") : 8;
      }
      return null;
    });
    await expect(defaultStorage()).rejects.toBe("plugin store absent");
    await expect(defaultStorage()).resolves.toBeDefined();
    expect(attempts).toBe(2);
  });
});
