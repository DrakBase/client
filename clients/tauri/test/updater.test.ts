import { clearMocks } from "@tauri-apps/api/mocks";
import { afterEach, describe, expect, it, vi } from "vitest";

import { saveSettings } from "../src/settings";
import {
  checkForUpdate,
  describeUpdateError,
  installUpdate,
  isWithdrawnUpdateError,
  updateChannel,
} from "../src/updater";
import { BYTES_RID, MemoryStorage, OS_VERSION, UPDATE_RID, fakeBackend, fixture } from "./support";

const UUID_V4 = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

afterEach(() => {
  clearMocks();
});

/** En-têtes passés au plugin, tels que `new Headers` les écrit : noms en minuscules. */
function sentHeaders(args: Record<string, unknown>): Record<string, string> {
  return Object.fromEntries(args["headers"] as [string, string][]);
}

describe("checkForUpdate", () => {
  it("envoie le canal, la version du système et l'identifiant d'installation en en-têtes", async () => {
    const backend = fakeBackend();
    const storage = new MemoryStorage();
    expect(await checkForUpdate({ storage })).toBeNull();
    const headers = sentHeaders(backend.last("plugin:updater|check"));
    expect(headers["x-appsmarket-channel"]).toBe("stable");
    expect(headers["x-appsmarket-install-id"]).toMatch(UUID_V4);
    expect(headers["x-appsmarket-install-id"]).toBe(storage.values.get("installId"));
    // La version vient du plugin os, sans que l'app la passe.
    expect(headers["x-appsmarket-os-version"]).toBe(OS_VERSION);
  });

  it("cherche sans la version du système quand le plugin os n'est pas enregistré", async () => {
    const backend = fakeBackend({ osVersion: null });
    await checkForUpdate({ storage: new MemoryStorage() });
    const headers = sentHeaders(backend.last("plugin:updater|check"));
    expect(headers["x-appsmarket-channel"]).toBe("stable");
    expect(headers).not.toHaveProperty("x-appsmarket-os-version");
  });

  it("n'envoie pas la version du système quand l'app passe une chaîne vide", async () => {
    const backend = fakeBackend();
    await checkForUpdate({ storage: new MemoryStorage(), osVersion: "" });
    expect(sentHeaders(backend.last("plugin:updater|check"))).not.toHaveProperty(
      "x-appsmarket-os-version",
    );
  });

  it("garde le même identifiant d'une recherche à l'autre", async () => {
    const backend = fakeBackend();
    const storage = new MemoryStorage();
    await checkForUpdate({ storage });
    const first = sentHeaders(backend.last("plugin:updater|check"))["x-appsmarket-install-id"];
    await checkForUpdate({ storage });
    const second = sentHeaders(backend.last("plugin:updater|check"))["x-appsmarket-install-id"];
    expect(second).toBe(first);
  });

  it("n'envoie aucun identifiant quand les statistiques sont coupées", async () => {
    const backend = fakeBackend();
    const storage = new MemoryStorage();
    await saveSettings({ channel: "beta", shareStatistics: false }, storage);
    await checkForUpdate({ storage });
    expect(sentHeaders(backend.last("plugin:updater|check"))).toEqual({
      "x-appsmarket-channel": "beta",
      "x-appsmarket-os-version": OS_VERSION,
    });
    expect(storage.values.has("installId")).toBe(false);
  });

  it("laisse les options de l'appel l'emporter sur les réglages", async () => {
    const backend = fakeBackend();
    const storage = new MemoryStorage();
    await checkForUpdate({
      storage,
      channel: "beta",
      shareStatistics: false,
      osVersion: "13.5",
      timeout: 15_000,
    });
    const args = backend.last("plugin:updater|check");
    expect(sentHeaders(args)).toEqual({
      "x-appsmarket-channel": "beta",
      "x-appsmarket-os-version": "13.5",
    });
    expect(args["timeout"]).toBe(15_000);
  });

  it("donne 60 s à la recherche par défaut, comme le paquet Swift", async () => {
    const backend = fakeBackend();
    await checkForUpdate({ storage: new MemoryStorage() });
    // Sans délai, le plugin attendrait sans fin un serveur qui ne répond pas.
    expect(backend.last("plugin:updater|check")["timeout"]).toBe(60_000);
  });

  it("renvoie la mise à jour proposée", async () => {
    fakeBackend({
      release: {
        version: "0.1.1",
        body: "- Correction",
        rawJson: { version: "0.1.1", channel: "stable", size: 1234 },
      },
    });
    const update = await checkForUpdate({ storage: new MemoryStorage() });
    expect(update?.version).toBe("0.1.1");
    expect(update?.body).toBe("- Correction");
    expect(update?.rawJson["size"]).toBe(1234);
  });

  it("traite une bêta reçue sur le canal stable comme « à jour »", async () => {
    const backend = fakeBackend({
      release: { version: "0.2.0-beta.1", rawJson: { version: "0.2.0-beta.1", channel: "beta" } },
    });
    expect(await checkForUpdate({ storage: new MemoryStorage() })).toBeNull();
    // La ressource de la mise à jour écartée est libérée côté Rust.
    expect(backend.last("plugin:resources|close")).toEqual({ rid: UPDATE_RID });
  });

  it("accepte une bêta sur le canal bêta", async () => {
    fakeBackend({
      release: { version: "0.2.0-beta.1", rawJson: { version: "0.2.0-beta.1", channel: "beta" } },
    });
    const update = await checkForUpdate({ storage: new MemoryStorage(), channel: "beta" });
    expect(update?.version).toBe("0.2.0-beta.1");
  });

  it("traite comme « à jour » une version qui exige un système plus récent", async () => {
    const backend = fakeBackend({
      release: {
        version: "0.2.0",
        rawJson: { version: "0.2.0", channel: "stable", min_os_version: "15.0" },
      },
    });
    // Le plugin os donne 14.6.1 : le serveur aurait dû écarter cette version.
    expect(await checkForUpdate({ storage: new MemoryStorage() })).toBeNull();
    expect(backend.last("plugin:resources|close")).toEqual({ rid: UPDATE_RID });

    // La version imposée par l'app compte de la même façon.
    const update = await checkForUpdate({ storage: new MemoryStorage(), osVersion: "15.0.1" });
    expect(update?.version).toBe("0.2.0");
    expect(await checkForUpdate({ storage: new MemoryStorage(), osVersion: "13.5" })).toBeNull();
  });

  it("n'écarte rien sans version du système, ou avec un minimum illisible", async () => {
    const release = (minimum: string) => ({
      version: "0.2.0",
      rawJson: { version: "0.2.0", channel: "stable", min_os_version: minimum },
    });
    fakeBackend({ release: release("15.0"), osVersion: null });
    expect((await checkForUpdate({ storage: new MemoryStorage() }))?.version).toBe("0.2.0");
    fakeBackend({ release: release("15.0") });
    const unsent = await checkForUpdate({ storage: new MemoryStorage(), osVersion: "" });
    expect(unsent?.version).toBe("0.2.0");
    fakeBackend({ release: release("quinze") });
    expect((await checkForUpdate({ storage: new MemoryStorage() }))?.version).toBe("0.2.0");
  });

  it("lit le canal des réponses de fixtures/update-response.json comme Rust et Swift", async () => {
    const { cases } = fixture<{
      cases: { name: string; body: unknown; expected: "ok" | "refused" }[];
    }>("update-response.json");
    const responses = cases.flatMap(({ name, body, expected }) =>
      typeof body === "object" && body !== null && !Array.isArray(body)
        ? [{ name, body: body as Record<string, unknown>, expected }]
        : [],
    );
    // Le plugin lit lui-même `version`, `url` et `signature` ; l'aide n'ajoute que la règle du canal.
    const accepted = responses.filter(({ expected }) => expected === "ok");
    const refused = responses.filter(({ name }) => ["sans channel", "canal inconnu"].includes(name));
    expect(accepted.length).toBeGreaterThan(1);
    expect(refused.map(({ expected }) => expected)).toEqual(["refused", "refused"]);

    for (const { name, body } of accepted) {
      const backend = fakeBackend({ release: { version: String(body["version"]), rawJson: body } });
      const channel = body["channel"] === "beta" ? "beta" : "stable";
      const update = await checkForUpdate({ storage: new MemoryStorage(), channel, osVersion: "" });
      expect(update && updateChannel(update), name).toBe(body["channel"]);
      expect(backend.commands(), name).not.toContain("plugin:resources|close");
    }
    for (const { name, body } of refused) {
      const backend = fakeBackend({ release: { version: String(body["version"]), rawJson: body } });
      await expect(
        checkForUpdate({ storage: new MemoryStorage(), channel: "beta" }),
        name,
      ).rejects.toThrow(/^réponse du serveur illisible : /);
      // La ressource de la réponse refusée est libérée côté Rust.
      expect(backend.last("plugin:resources|close"), name).toEqual({ rid: UPDATE_RID });
    }
  });

  it("transmet l'erreur du plugin", async () => {
    fakeBackend({
      failures: { "plugin:updater|check": "Could not fetch a valid release JSON from the remote" },
    });
    await expect(checkForUpdate({ storage: new MemoryStorage() })).rejects.toBe(
      "Could not fetch a valid release JSON from the remote",
    );
  });
});

describe("updateChannel", () => {
  it("rend le canal annoncé par le serveur, stable à défaut", () => {
    expect(updateChannel({ rawJson: { version: "0.2.0-beta.1", channel: "beta" } })).toBe("beta");
    expect(updateChannel({ rawJson: { version: "0.2.0", channel: "stable" } })).toBe("stable");
    // Un champ absent ou inconnu vaut stable, comme une valeur inconnue côté serveur.
    expect(updateChannel({ rawJson: { version: "0.2.0" } })).toBe("stable");
    expect(updateChannel({ rawJson: { version: "0.2.0", channel: "nightly" } })).toBe("stable");
  });
});

describe("installUpdate", () => {
  it("télécharge sans en-tête de l'app, installe, puis relance", async () => {
    const backend = fakeBackend({
      release: { version: "0.1.1" },
      downloadEvents: [
        { event: "Started", data: { contentLength: 300 } },
        { event: "Progress", data: { chunkLength: 100 } },
        { event: "Progress", data: { chunkLength: 200 } },
        { event: "Finished" },
      ],
    });
    const update = await checkForUpdate({ storage: new MemoryStorage() });
    const progress = vi.fn();
    const beforeInstall = vi.fn(() => {
      // L'archive est déjà téléchargée et vérifiée, rien n'est encore installé.
      expect(backend.commands()).toContain("plugin:updater|download");
      expect(backend.commands()).not.toContain("plugin:updater|install");
    });
    await installUpdate(update!, { onProgress: progress, beforeInstall });

    expect(progress.mock.calls.map(([value]) => value as unknown)).toEqual([
      { downloaded: 0, total: 300 },
      { downloaded: 100, total: 300 },
      { downloaded: 300, total: 300 },
      { downloaded: 300, total: 300 },
    ]);
    expect(beforeInstall).toHaveBeenCalledOnce();
    // Quatre heures pour télécharger, comme le paquet Swift : sans délai, le plugin attendrait sans
    // fin un serveur qui cesse d'envoyer.
    expect(backend.last("plugin:updater|download")).toMatchObject({
      rid: UPDATE_RID,
      headers: [],
      timeout: 4 * 3_600_000,
    });
    expect(backend.last("plugin:updater|install")).toEqual({
      updateRid: UPDATE_RID,
      bytesRid: BYTES_RID,
    });
    expect(backend.commands().slice(-3)).toEqual([
      "plugin:updater|download",
      "plugin:updater|install",
      "plugin:process|restart",
    ]);
  });

  it("donne au téléchargement le délai demandé", async () => {
    const backend = fakeBackend({ release: { version: "0.1.1" } });
    const update = await checkForUpdate({ storage: new MemoryStorage() });
    await installUpdate(update!, { downloadTimeout: 900_000 });
    expect(backend.last("plugin:updater|download")["timeout"]).toBe(900_000);
  });

  it("n'installe rien si le téléchargement échoue", async () => {
    const backend = fakeBackend({
      release: { version: "0.1.1" },
      failures: { "plugin:updater|download": "`Download request failed with status: 410 Gone`" },
    });
    const update = await checkForUpdate({ storage: new MemoryStorage() });
    await expect(installUpdate(update!)).rejects.toMatch("410");
    expect(backend.commands()).not.toContain("plugin:updater|install");
    expect(backend.commands()).not.toContain("plugin:process|restart");
  });

  it("n'installe rien si l'app refuse juste avant l'installation", async () => {
    const backend = fakeBackend({ release: { version: "0.1.1" } });
    const update = await checkForUpdate({ storage: new MemoryStorage() });
    const refusal = new Error("brouillon non enregistré");
    await expect(
      installUpdate(update!, {
        beforeInstall: () => {
          throw refusal;
        },
      }),
    ).rejects.toBe(refusal);
    expect(backend.commands()).not.toContain("plugin:updater|install");
  });
});

describe("describeUpdateError", () => {
  it("traduit les erreurs courantes du plugin, en gardant le texte d'origine", () => {
    expect(describeUpdateError("`Download request failed with status: 410 Gone`")).toBe(
      "cette version a été retirée par l'éditeur ; recherche à nouveau (Download request failed with status: 410 Gone)",
    );
    expect(
      describeUpdateError(
        "The update was signed for version 0.1.1 but the update endpoint announced version 0.1.5. The endpoint response may have been tampered with to force installing a different release.",
      ),
    ).toMatch(/^mise à jour refusée : la version signée n'est pas celle annoncée/);
    expect(describeUpdateError("The signature verification failed")).toMatch(
      /^mise à jour refusée : signature invalide/,
    );
    // Un téléchargement coupé, ou plus long que son délai.
    expect(describeUpdateError("error decoding response body")).toBe(
      "réponse du serveur interrompue, trop longue à venir ou illisible (error decoding response body)",
    );
    expect(describeUpdateError(new Error("autre chose"))).toBe("autre chose");
  });

  it("reconnaît une version retirée pendant le téléchargement", () => {
    expect(isWithdrawnUpdateError("`Download request failed with status: 410 Gone`")).toBe(true);
    expect(isWithdrawnUpdateError("`Download request failed with status: 404 Not Found`")).toBe(
      false,
    );
    expect(isWithdrawnUpdateError("The signature verification failed")).toBe(false);
  });
});
