import type { UpdateChannel } from "@drakbase/tauri";
import { useUpdater, type UseUpdaterResult } from "@drakbase/tauri/react";
import { useEffect, useRef, useState } from "react";

import { journal, launchOptions, type LaunchOptions } from "./e2e";

export function App() {
  const updater = useUpdater({
    // La première recherche attend les options d'essai, qui peuvent changer les réglages.
    checkOnMount: false,
    beforeInstall: () => {
      journal("before-install");
    },
  });
  const { phase, update, error, progress, check, install, setChannel, setShareStatistics } =
    updater;
  const [options, setOptions] = useState<LaunchOptions | null>(null);

  const started = useRef(false);
  useEffect(() => {
    if (started.current) {
      return;
    }
    started.current = true;
    void (async () => {
      const launch = await launchOptions().catch(() => null);
      if (launch?.channel) {
        await setChannel(launch.channel);
      }
      if (launch?.shareStatistics != null) {
        await setShareStatistics(launch.shareStatistics);
      }
      setOptions(launch);
      await check();
    })();
  }, [check, setChannel, setShareStatistics]);

  // En mode d'essai : chaque changement d'étape va au journal, et la mise à jour s'installe seule.
  const lastEntry = useRef("");
  useEffect(() => {
    if (!options?.e2e) {
      return;
    }
    const key = `${phase}|${update?.version ?? ""}|${error ?? ""}`;
    if (key !== lastEntry.current) {
      lastEntry.current = key;
      journal("phase", { phase, update, error, progress });
    }
  }, [options, phase, update, error, progress]);

  useEffect(() => {
    if (options?.autoUpdate && phase === "available") {
      void install();
    }
  }, [options, phase, install]);

  return (
    <main className="page">
      <header className="header">
        <img className="icon" src="/icon.svg" alt="" width={56} height={56} />
        <div>
          <h1>Démo Tauri</h1>
          <p className="version">Version {updater.currentVersion ?? "…"}</p>
        </div>
      </header>
      <UpdatePanel updater={updater} />
      {options?.e2e ? <p className="e2e">Mode d'essai de bout en bout</p> : null}
    </main>
  );
}

function UpdatePanel({ updater }: { updater: UseUpdaterResult }) {
  const { phase, settings, update, progress, error, lastCheck } = updater;
  const busy = phase === "checking" || phase === "downloading" || phase === "installing";

  return (
    <section className="panel" aria-labelledby="titre-maj">
      <h2 id="titre-maj">Mises à jour</h2>
      <p className="status" role="status">
        {statusText(updater)}
      </p>

      {update !== null && (phase === "available" || phase === "error") ? (
        <div className="update">
          <p className="update-title">
            Version {update.version}
            {update.channel === "beta" ? <span className="badge">bêta</span> : null}
          </p>
          {update.notes ? <pre className="notes">{update.notes}</pre> : null}
          <button type="button" onClick={() => void updater.install()} disabled={busy}>
            Installer et relancer
          </button>
        </div>
      ) : null}

      {phase === "downloading" && progress !== null ? (
        <progress
          className="progress"
          max={progress.total ?? undefined}
          value={progress.total === null ? undefined : progress.downloaded}
        />
      ) : null}

      {phase === "error" && error !== null ? <p className="error">{error}</p> : null}

      <div className="actions">
        <button type="button" onClick={() => void updater.check()} disabled={busy}>
          Rechercher les mises à jour
        </button>
      </div>

      <div className="settings">
        <label className="field">
          <span>Canal</span>
          <select
            value={settings?.channel ?? "stable"}
            disabled={settings === null || busy}
            onChange={(event) => {
              const channel = event.target.value === "beta" ? "beta" : "stable";
              void changeChannel(updater, channel);
            }}
          >
            <option value="stable">Stable</option>
            <option value="beta">Bêta</option>
          </select>
        </label>
        <label className="check">
          <input
            type="checkbox"
            checked={settings?.shareStatistics ?? true}
            disabled={settings === null}
            onChange={(event) => void updater.setShareStatistics(event.target.checked)}
          />
          <span>
            Envoyer des statistiques anonymes
            <small>
              Un identifiant tiré au hasard, sans lien avec cet ordinateur, compte les installations
              actives. Le couper l'efface.
            </small>
          </span>
        </label>
      </div>

      {lastCheck !== null ? (
        <p className="last-check">Dernière recherche : {lastCheck.toLocaleString("fr-FR")}</p>
      ) : null}
    </section>
  );
}

async function changeChannel(updater: UseUpdaterResult, channel: UpdateChannel) {
  await updater.setChannel(channel);
  await updater.check();
}

function statusText({ phase, currentVersion, update, progress }: UseUpdaterResult): string {
  switch (phase) {
    case "idle":
      return "Pas encore de recherche.";
    case "checking":
      return "Recherche d'une mise à jour…";
    case "upToDate":
      return `L'app est à jour${currentVersion ? ` (version ${currentVersion})` : ""}.`;
    case "available":
      return `La version ${update?.version ?? ""} est disponible.`;
    case "downloading":
      return progress?.total
        ? `Téléchargement… ${Math.floor((progress.downloaded / progress.total) * 100)} %`
        : "Téléchargement…";
    case "installing":
      return "Installation, puis relance…";
    case "error":
      return "La mise à jour n'a pas abouti.";
  }
}
