import { clearMocks } from "@tauri-apps/api/mocks";
import { act, renderHook, waitFor } from "@testing-library/react";
import { StrictMode, type ReactNode } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useUpdater } from "../src/react";
import {
  MemoryStorage,
  UPDATE_RID,
  fakeBackend,
  type FakeBackend,
  type FakeBackendOptions,
} from "./support";

afterEach(() => {
  clearMocks();
});

/** Recherches reçues par le plugin. */
function checks(backend: FakeBackend): string[] {
  return backend.commands().filter((cmd) => cmd === "plugin:updater|check");
}

const RELEASE = {
  version: "0.1.1",
  body: "- Correction du démarrage",
  date: "2026-10-25T10:00:00Z",
  rawJson: { version: "0.1.1", channel: "stable" },
};

describe("useUpdater", () => {
  it("lit la version et les réglages, puis cherche au montage", async () => {
    const backend = fakeBackend({ release: RELEASE, version: "0.1.0" });
    const { result } = renderHook(() => useUpdater({ storage: new MemoryStorage() }));
    await waitFor(() => {
      expect(result.current.phase).toBe("available");
    });
    expect(result.current.currentVersion).toBe("0.1.0");
    expect(result.current.settings).toEqual({ channel: "stable", shareStatistics: true });
    expect(result.current.update).toEqual({
      version: "0.1.1",
      currentVersion: "0.1.0",
      notes: "- Correction du démarrage",
      date: "2026-10-25T10:00:00Z",
      channel: "stable",
    });
    expect(result.current.lastCheck).toBeInstanceOf(Date);
    expect(backend.commands().filter((cmd) => cmd === "plugin:updater|check")).toHaveLength(1);
  });

  it("ne cherche qu'une fois au montage en mode strict", async () => {
    const backend = fakeBackend();
    const wrapper = ({ children }: { children: ReactNode }) => <StrictMode>{children}</StrictMode>;
    const { result } = renderHook(() => useUpdater({ storage: new MemoryStorage() }), { wrapper });
    await waitFor(() => {
      expect(result.current.phase).toBe("upToDate");
    });
    expect(backend.commands().filter((cmd) => cmd === "plugin:updater|check")).toHaveLength(1);
  });

  it("attend qu'on le lui demande avec checkOnMount à faux", async () => {
    const backend = fakeBackend();
    const { result } = renderHook(() =>
      useUpdater({ storage: new MemoryStorage(), checkOnMount: false }),
    );
    await waitFor(() => {
      expect(result.current.settings).not.toBeNull();
    });
    expect(result.current.phase).toBe("idle");
    expect(backend.commands()).not.toContain("plugin:updater|check");
    await act(() => result.current.check());
    expect(result.current.phase).toBe("upToDate");
  });

  it("installe avec la progression, puis relance", async () => {
    const backend = fakeBackend({
      release: RELEASE,
      downloadEvents: [
        { event: "Started", data: { contentLength: 10 } },
        { event: "Progress", data: { chunkLength: 10 } },
        { event: "Finished" },
      ],
    });
    const beforeInstall = vi.fn();
    const { result } = renderHook(() =>
      useUpdater({ storage: new MemoryStorage(), beforeInstall }),
    );
    await waitFor(() => {
      expect(result.current.phase).toBe("available");
    });
    await act(() => result.current.install());
    expect(result.current.phase).toBe("installing");
    expect(result.current.progress).toEqual({ downloaded: 10, total: 10 });
    expect(beforeInstall).toHaveBeenCalledOnce();
    expect(backend.commands().slice(-3)).toEqual([
      "plugin:updater|download",
      "plugin:updater|install",
      "plugin:process|restart",
    ]);
  });

  it("oublie une version retirée pendant son téléchargement", async () => {
    const backend = fakeBackend({
      release: RELEASE,
      failures: { "plugin:updater|download": "`Download request failed with status: 410 Gone`" },
    });
    const { result } = renderHook(() => useUpdater({ storage: new MemoryStorage() }));
    await waitFor(() => {
      expect(result.current.phase).toBe("available");
    });
    await act(() => result.current.install());
    expect(result.current.phase).toBe("error");
    expect(result.current.error).toMatch(
      /^Installation impossible : cette version a été retirée par l'éditeur/,
    );
    expect(result.current.update).toBeNull();
    expect(backend.commands()).toContain("plugin:resources|close");
    // Sans mise à jour en attente, installer ne fait plus rien.
    await act(() => result.current.install());
    expect(backend.commands().filter((cmd) => cmd === "plugin:updater|download")).toHaveLength(1);
  });

  it("garde la mise à jour après une autre erreur, pour réessayer", async () => {
    const backend = fakeBackend({
      release: RELEASE,
      failures: { "plugin:updater|download": "The signature verification failed" },
    });
    const { result } = renderHook(() => useUpdater({ storage: new MemoryStorage() }));
    await waitFor(() => {
      expect(result.current.phase).toBe("available");
    });
    await act(() => result.current.install());
    expect(result.current.error).toMatch(/^Installation impossible : mise à jour refusée/);
    expect(result.current.update?.version).toBe("0.1.1");
    await act(() => result.current.install());
    expect(backend.commands().filter((cmd) => cmd === "plugin:updater|download")).toHaveLength(2);
  });

  it("donne quatre heures au téléchargement, ou le délai de downloadTimeout", async () => {
    const backend = fakeBackend({ release: RELEASE });
    const { result, rerender } = renderHook(
      ({ downloadTimeout }: { downloadTimeout?: number }) =>
        useUpdater({ storage: new MemoryStorage(), downloadTimeout }),
      { initialProps: {} },
    );
    await waitFor(() => {
      expect(result.current.phase).toBe("available");
    });
    await act(() => result.current.install());
    // Sans délai, un téléchargement figé laisserait le crochet occupé pour toujours.
    expect(backend.last("plugin:updater|download")["timeout"]).toBe(4 * 3_600_000);

    rerender({ downloadTimeout: 600_000 });
    await act(() => result.current.install());
    expect(backend.last("plugin:updater|download")["timeout"]).toBe(600_000);
  });

  it("rend la main quand le téléchargement dépasse son délai", async () => {
    const backend = fakeBackend({
      release: RELEASE,
      failures: {
        "plugin:updater|download":
          "error decoding response body for url (https://maj.example.fr/dl/01JB8Z6T3KQ4N5V7W9X0Y1Z2A3/demo.app.tar.gz)",
      },
    });
    const { result } = renderHook(() => useUpdater({ storage: new MemoryStorage() }));
    await waitFor(() => {
      expect(result.current.phase).toBe("available");
    });
    await act(() => result.current.install());
    expect(result.current.phase).toBe("error");
    expect(result.current.error).toMatch(
      /^Installation impossible : réponse du serveur interrompue, trop longue à venir ou illisible/,
    );
    // La version reste à installer, et le crochet n'est plus occupé : une recherche part.
    expect(result.current.update?.version).toBe("0.1.1");
    await act(() => result.current.check());
    expect(checks(backend)).toHaveLength(2);
  });

  it("montre une erreur quand la recherche échoue", async () => {
    fakeBackend({
      failures: { "plugin:updater|check": "Could not fetch a valid release JSON from the remote" },
    });
    const { result } = renderHook(() => useUpdater({ storage: new MemoryStorage() }));
    await waitFor(() => {
      expect(result.current.phase).toBe("error");
    });
    expect(result.current.error).toMatch(/^Recherche impossible : le serveur de mises à jour/);
    expect(result.current.update).toBeNull();
  });

  it("rend la main quand la recherche dépasse son délai", async () => {
    const backend = fakeBackend({
      failures: {
        "plugin:updater|check":
          "error sending request for url (https://maj.example.fr/api/v1/update/demo/darwin/aarch64/0.1.0)",
      },
    });
    const { result } = renderHook(() => useUpdater({ storage: new MemoryStorage() }));
    await waitFor(() => {
      expect(result.current.phase).toBe("error");
    });
    // Le délai est celui du plugin : passé ce temps, il rejette la recherche.
    expect(backend.last("plugin:updater|check")["timeout"]).toBe(60_000);
    expect(result.current.error).toMatch(
      /^Recherche impossible : serveur de mises à jour injoignable/,
    );
    // Le crochet n'est plus occupé : une nouvelle recherche part.
    await act(() => result.current.check());
    expect(checks(backend)).toHaveLength(2);
  });

  it("n'avance lastCheck qu'après une recherche réussie", async () => {
    const options: FakeBackendOptions = {};
    fakeBackend(options);
    const { result } = renderHook(() => useUpdater({ storage: new MemoryStorage() }));
    await waitFor(() => {
      expect(result.current.phase).toBe("upToDate");
    });
    const succeeded = result.current.lastCheck;
    expect(succeeded).toBeInstanceOf(Date);

    options.failures = { "plugin:updater|check": "error sending request for url (…)" };
    await act(() => result.current.check());
    expect(result.current.phase).toBe("error");
    expect(result.current.lastCheck).toBe(succeeded);
  });

  it("garde la version proposée quand une recherche demandée échoue", async () => {
    const options: FakeBackendOptions = { release: RELEASE };
    const backend = fakeBackend(options);
    const { result } = renderHook(() => useUpdater({ storage: new MemoryStorage() }));
    await waitFor(() => {
      expect(result.current.phase).toBe("available");
    });

    options.failures = { "plugin:updater|check": "error sending request for url (…)" };
    await act(() => result.current.check());
    // L'erreur est dite, mais la version déjà proposée reste à installer.
    expect(result.current.phase).toBe("error");
    expect(result.current.error).toMatch(/^Recherche impossible : serveur de mises à jour/);
    expect(result.current.update?.version).toBe("0.1.1");
    expect(backend.commands()).not.toContain("plugin:resources|close");

    options.failures = {};
    await act(() => result.current.install());
    expect(backend.commands()).toContain("plugin:updater|download");
  });

  it("refait la recherche quand le canal change pendant qu'elle court", async () => {
    let answer: (release: null) => void = () => undefined;
    const pendingAnswer = new Promise<null>((resolve) => {
      answer = resolve;
    });
    const beta = { version: "0.2.0-beta.1", rawJson: { version: "0.2.0-beta.1", channel: "beta" } };
    // Le canal bêta reçoit une bêta, quand le test la laisse partir ; le canal stable n'a rien.
    const backend = fakeBackend({
      release: (channel) => (channel === "beta" ? pendingAnswer.then(() => beta) : null),
    });
    const storage = new MemoryStorage();
    storage.values.set("channel", "beta");
    const { result } = renderHook(() => useUpdater({ storage }));
    await waitFor(() => {
      expect(checks(backend)).toHaveLength(1);
    });

    await act(() => result.current.setChannel("stable"));
    expect(result.current.settings?.channel).toBe("stable");
    expect(result.current.phase).toBe("checking");
    answer(null);

    // La bêta, reçue pour l'ancien canal, n'est pas proposée : elle est libérée, et la recherche
    // repart sur le canal stable.
    await waitFor(() => {
      expect(result.current.phase).toBe("upToDate");
    });
    expect(result.current.update).toBeNull();
    expect(checks(backend)).toHaveLength(2);
    const headers = Object.fromEntries(
      backend.last("plugin:updater|check")["headers"] as [string, string][],
    );
    expect(headers["x-appsmarket-channel"]).toBe("stable");
    expect(backend.commands()).toContain("plugin:resources|close");
  });

  describe("canal changé pendant une opération qui échoue", () => {
    const BETA = { version: "0.2.0-beta.1", rawJson: { version: "0.2.0-beta.1", channel: "beta" } };

    /** Une porte : ce qui l'attend reste en vol jusqu'à `open()`. */
    function gate(): { closed: Promise<void>; open: () => void } {
      let open: () => void = () => undefined;
      const closed = new Promise<void>((resolve) => {
        open = resolve;
      });
      return { closed, open };
    }

    /** Un crochet sur le canal bêta, avec une bêta proposée. */
    async function withBetaAvailable(options: FakeBackendOptions) {
      const backend = fakeBackend(options);
      const storage = new MemoryStorage();
      storage.values.set("channel", "beta");
      const hook = renderHook(() => useUpdater({ storage }));
      await waitFor(() => {
        expect(hook.result.current.phase).toBe("available");
      });
      expect(hook.result.current.update?.channel).toBe("beta");
      return { backend, result: hook.result };
    }

    it("oublie la bêta proposée quand la recherche échoue", async () => {
      const options: FakeBackendOptions = { release: BETA };
      const { backend, result } = await withBetaAvailable(options);

      // Une recherche demandée part et reste en vol ; le canal revient à stable ; elle échoue.
      const { closed, open } = gate();
      options.gates = { "plugin:updater|check": closed };
      options.failures = { "plugin:updater|check": "error sending request for url (…)" };
      act(() => {
        void result.current.check();
      });
      await waitFor(() => {
        expect(checks(backend)).toHaveLength(2);
      });
      await act(() => result.current.setChannel("stable"));
      open();
      await waitFor(() => {
        expect(result.current.phase).toBe("error");
      });

      // La bêta venait de l'ancien canal : elle n'est plus proposée, ni installable.
      expect(result.current.update).toBeNull();
      expect(backend.commands()).toContain("plugin:resources|close");
      await act(() => result.current.install());
      expect(backend.commands()).not.toContain("plugin:updater|download");
    });

    it("oublie la bêta proposée quand son installation échoue", async () => {
      const { closed, open } = gate();
      const options: FakeBackendOptions = {
        release: BETA,
        gates: { "plugin:updater|download": closed },
        failures: { "plugin:updater|download": "error decoding response body" },
      };
      const { backend, result } = await withBetaAvailable(options);

      act(() => {
        void result.current.install();
      });
      await waitFor(() => {
        expect(backend.commands()).toContain("plugin:updater|download");
      });
      await act(() => result.current.setChannel("stable"));
      open();
      await waitFor(() => {
        expect(result.current.phase).toBe("error");
      });

      expect(result.current.update).toBeNull();
      expect(backend.commands()).toContain("plugin:resources|close");
      await act(() => result.current.install());
      expect(backend.commands().filter((cmd) => cmd === "plugin:updater|download")).toHaveLength(1);
    });
  });

  describe("recherche périodique", () => {
    const HOUR = 3_600_000;

    /** Laisse passer `ms` millisecondes, avec les recherches qui en découlent. */
    async function elapse(ms: number): Promise<void> {
      await act(async () => {
        await vi.advanceTimersByTimeAsync(ms);
      });
    }

    beforeEach(() => {
      vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
    });

    afterEach(() => {
      vi.useRealTimers();
    });

    it("cherche de nouveau un jour après la dernière recherche, manuelle comprise", async () => {
      const backend = fakeBackend();
      const { result } = renderHook(() => useUpdater({ storage: new MemoryStorage() }));
      await elapse(0);
      expect(result.current.phase).toBe("upToDate");
      expect(checks(backend)).toHaveLength(1);

      await elapse(24 * HOUR - 1_000);
      expect(checks(backend)).toHaveLength(1);
      await elapse(1_000);
      expect(checks(backend)).toHaveLength(2);
      expect(result.current.phase).toBe("upToDate");

      // Une recherche demandée repousse la suivante d'un intervalle entier.
      await elapse(12 * HOUR);
      await act(() => result.current.check());
      expect(checks(backend)).toHaveLength(3);
      await elapse(24 * HOUR - 1_000);
      expect(checks(backend)).toHaveLength(3);
      await elapse(1_000);
      expect(checks(backend)).toHaveLength(4);
    });

    it("ramène l'intervalle entre une heure et trente jours", async () => {
      const backend = fakeBackend();
      const { rerender } = renderHook(
        ({ checkInterval }: { checkInterval: number }) =>
          useUpdater({ storage: new MemoryStorage(), checkInterval }),
        { initialProps: { checkInterval: 60_000 } },
      );
      await elapse(0);
      expect(checks(backend)).toHaveLength(1);
      await elapse(HOUR - 1_000);
      expect(checks(backend)).toHaveLength(1);
      await elapse(1_000);
      expect(checks(backend)).toHaveLength(2);

      rerender({ checkInterval: 365 * 24 * HOUR });
      await elapse(30 * 24 * HOUR - 1_000);
      expect(checks(backend)).toHaveLength(2);
      await elapse(1_000);
      expect(checks(backend)).toHaveLength(3);
    });

    it.each([0, null])("ne cherche qu'au montage avec checkInterval à %s", async (checkInterval) => {
      const backend = fakeBackend();
      renderHook(() => useUpdater({ storage: new MemoryStorage(), checkInterval }));
      await elapse(0);
      expect(checks(backend)).toHaveLength(1);
      await elapse(40 * 24 * HOUR);
      expect(checks(backend)).toHaveLength(1);
    });

    it("compte depuis le montage quand rien n'a encore été cherché", async () => {
      const backend = fakeBackend();
      renderHook(() => useUpdater({ storage: new MemoryStorage(), checkOnMount: false }));
      await elapse(24 * HOUR - 1_000);
      expect(checks(backend)).toHaveLength(0);
      await elapse(1_000);
      expect(checks(backend)).toHaveLength(1);
    });

    it("garde la version proposée quand une recherche automatique échoue", async () => {
      const options: FakeBackendOptions = { release: RELEASE };
      const backend = fakeBackend(options);
      const { result } = renderHook(() => useUpdater({ storage: new MemoryStorage() }));
      await elapse(0);
      expect(result.current.phase).toBe("available");
      const succeeded = result.current.lastCheck;

      options.failures = { "plugin:updater|check": "error sending request for url (…)" };
      await elapse(24 * HOUR);
      expect(checks(backend)).toHaveLength(2);
      expect(result.current.phase).toBe("available");
      expect(result.current.error).toBeNull();
      expect(result.current.update?.version).toBe("0.1.1");
      expect(result.current.lastCheck).toBe(succeeded);
      // La proposition n'a pas été libérée : elle s'installe encore.
      expect(backend.commands()).not.toContain("plugin:resources|close");
      await act(() => result.current.install());
      expect(backend.commands()).toContain("plugin:updater|download");
    });

    it("ne repousse pas la recherche au-delà d'un intervalle quand l'horloge recule", async () => {
      const backend = fakeBackend();
      renderHook(() => useUpdater({ storage: new MemoryStorage() }));
      await elapse(0);
      expect(checks(backend)).toHaveLength(1);

      // L'horloge recule de dix jours : la dernière recherche paraît faite dans le futur. Au
      // réveil suivant, dans l'heure, l'attente repart pour un intervalle, pas pour onze jours.
      vi.setSystemTime(Date.now() - 10 * 24 * HOUR);
      await elapse(24 * HOUR - 1_000);
      expect(checks(backend)).toHaveLength(1);
      await elapse(2 * HOUR);
      expect(checks(backend)).toHaveLength(2);
    });

    it("s'arrête au démontage", async () => {
      const backend = fakeBackend();
      const { unmount } = renderHook(() => useUpdater({ storage: new MemoryStorage() }));
      await elapse(0);
      unmount();
      await elapse(3 * 24 * HOUR);
      expect(checks(backend)).toHaveLength(1);
    });
  });

  it("enregistre le canal et oublie la mise à jour affichée", async () => {
    const backend = fakeBackend({ release: RELEASE });
    const storage = new MemoryStorage();
    const { result } = renderHook(() => useUpdater({ storage }));
    await waitFor(() => {
      expect(result.current.phase).toBe("available");
    });
    await act(() => result.current.setChannel("beta"));
    expect(result.current.phase).toBe("idle");
    expect(result.current.update).toBeNull();
    expect(result.current.settings?.channel).toBe("beta");
    expect(storage.values.get("channel")).toBe("beta");
    expect(backend.commands()).toContain("plugin:resources|close");
    await act(() => result.current.check());
    const headers = Object.fromEntries(
      backend.last("plugin:updater|check")["headers"] as [string, string][],
    );
    expect(headers["x-appsmarket-channel"]).toBe("beta");
  });

  it("coupe les statistiques et efface l'identifiant", async () => {
    fakeBackend();
    const storage = new MemoryStorage();
    const { result } = renderHook(() => useUpdater({ storage }));
    await waitFor(() => {
      expect(result.current.phase).toBe("upToDate");
    });
    expect(storage.values.has("installId")).toBe(true);
    await act(() => result.current.setShareStatistics(false));
    expect(result.current.settings?.shareStatistics).toBe(false);
    expect(storage.values.has("installId")).toBe(false);
  });

  describe("démontage pendant une recherche", () => {
    /** Un crochet dont la première recherche reste en vol jusqu'à `answer()`. */
    async function withCheckInFlight() {
      let answer: () => void = () => undefined;
      const held = new Promise<void>((resolve) => {
        answer = resolve;
      });
      const backend = fakeBackend({ release: () => held.then(() => RELEASE) });
      const hook = renderHook(() => useUpdater({ storage: new MemoryStorage() }));
      await waitFor(() => {
        expect(checks(backend)).toHaveLength(1);
      });
      return { backend, answer, result: hook.result, unmount: hook.unmount };
    }

    it("libère la réponse reçue ensuite, que plus personne ne proposera", async () => {
      const { backend, answer, unmount } = await withCheckInFlight();
      unmount();
      expect(backend.commands()).not.toContain("plugin:resources|close");
      answer();
      await waitFor(() => {
        expect(backend.last("plugin:resources|close")).toEqual({ rid: UPDATE_RID });
      });
    });

    it("ne relance pas la recherche, même si le canal a changé", async () => {
      const { backend, answer, result, unmount } = await withCheckInFlight();
      await act(() => result.current.setChannel("beta"));
      unmount();
      answer();
      await waitFor(() => {
        expect(backend.commands()).toContain("plugin:resources|close");
      });
      // Le temps qu'une seconde recherche serait partie.
      await new Promise((resolve) => setTimeout(resolve, 20));
      expect(checks(backend)).toHaveLength(1);
    });
  });

  it("libère la mise à jour au démontage", async () => {
    const backend = fakeBackend({ release: RELEASE });
    const { result, unmount } = renderHook(() => useUpdater({ storage: new MemoryStorage() }));
    await waitFor(() => {
      expect(result.current.phase).toBe("available");
    });
    unmount();
    await waitFor(() => {
      expect(backend.commands()).toContain("plugin:resources|close");
    });
  });
});
