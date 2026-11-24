/**
 * Crochet React `useUpdater` : l'état d'une mise à jour (recherche, téléchargement,
 * installation), les réglages de l'utilisateur et les actions, pour bâtir une interface.
 */
import { getVersion } from "@tauri-apps/api/app";
import type { Update } from "@tauri-apps/plugin-updater";
import { useCallback, useEffect, useRef, useState } from "react";

import {
  loadSettings,
  saveSettings,
  type SettingsStorage,
  type UpdateChannel,
  type UpdaterSettings,
} from "./settings";
import {
  checkForUpdate,
  describeUpdateError,
  installUpdate,
  isWithdrawnUpdateError,
  updateChannel,
  type DownloadProgress,
} from "./updater";

export type UpdaterPhase =
  | "idle"
  | "checking"
  | "upToDate"
  | "available"
  | "downloading"
  | "installing"
  | "error";

/** La version proposée, en données simples. */
export interface AvailableUpdate {
  version: string;
  currentVersion: string;
  /** Notes de version en Markdown, chaîne vide si le serveur n'en donne pas. */
  notes: string;
  /** Date de publication, en RFC 3339. */
  date: string | null;
  channel: UpdateChannel;
}

export interface UseUpdaterOptions {
  /** Cherche une mise à jour dès le montage ; vrai par défaut. */
  checkOnMount?: boolean | undefined;
  /**
   * Intervalle entre deux recherches automatiques, en millisecondes : un jour par défaut, ramené
   * entre une heure et trente jours, comme dans le paquet Swift. Il court depuis la dernière
   * recherche, demandée ou non. `0` ou `null` : l'app ne cherche qu'au montage et à la demande.
   */
  checkInterval?: number | null | undefined;
  /** Version de l'OS envoyée au serveur ; par défaut, celle du plugin os, voir `checkForUpdate`. */
  osVersion?: string | undefined;
  /** Délai de la recherche, en millisecondes ; 60 000 par défaut, voir `checkForUpdate`. */
  timeout?: number | undefined;
  /** Délai du téléchargement, en millisecondes ; 4 h par défaut, voir `installUpdate`. */
  downloadTimeout?: number | undefined;
  /** Appelé juste avant l'installation, voir `installUpdate`. */
  beforeInstall?: (() => void | Promise<void>) | undefined;
  /** Rangement des réglages ; par défaut, `appsmarket.json` dans les données de l'app. */
  storage?: SettingsStorage | undefined;
}

export interface UpdaterState {
  phase: UpdaterPhase;
  /** Version de l'app en cours d'exécution. */
  currentVersion: string | null;
  /** Réglages enregistrés ; `null` pendant leur lecture. */
  settings: UpdaterSettings | null;
  /**
   * Version proposée, encore à installer. Elle reste connue dans la phase `error`, après une
   * recherche ou une installation qui échoue, sauf si l'éditeur l'a retirée.
   */
  update: AvailableUpdate | null;
  progress: DownloadProgress | null;
  /** Message en français, renseigné dans la phase `error`. */
  error: string | null;
  /** Dernière recherche réussie : une recherche qui échoue ne la change pas. */
  lastCheck: Date | null;
}

export interface UseUpdaterResult extends UpdaterState {
  /** Cherche une mise à jour ; sans effet pendant une recherche ou une installation. */
  check: () => Promise<void>;
  /** Télécharge, vérifie et installe la mise à jour proposée, puis relance l'app. */
  install: () => Promise<void>;
  /**
   * Change le canal ; la mise à jour affichée est oubliée, il faut chercher à nouveau. Pendant une
   * recherche, elle est refaite d'elle-même avec le nouveau canal.
   */
  setChannel: (channel: UpdateChannel) => Promise<void>;
  /** Active ou coupe les statistiques anonymes (les couper efface l'identifiant). */
  setShareStatistics: (enabled: boolean) => Promise<void>;
}

const INITIAL_STATE: UpdaterState = {
  phase: "idle",
  currentVersion: null,
  settings: null,
  update: null,
  progress: null,
  error: null,
  lastCheck: null,
};

const HOUR = 3_600_000;
/** Intervalle par défaut entre deux recherches automatiques, et ses bornes : celles du paquet Swift. */
const DEFAULT_CHECK_INTERVAL = 24 * HOUR;
const MINIMUM_CHECK_INTERVAL = HOUR;
const MAXIMUM_CHECK_INTERVAL = 30 * 24 * HOUR;

/** Intervalle des recherches automatiques, en millisecondes, ou `null` s'il n'y en a pas. */
function effectiveCheckInterval(value: number | null | undefined): number | null {
  if (value === null || value === 0) {
    return null;
  }
  if (value === undefined || !Number.isFinite(value)) {
    return DEFAULT_CHECK_INTERVAL;
  }
  return Math.min(Math.max(value, MINIMUM_CHECK_INTERVAL), MAXIMUM_CHECK_INTERVAL);
}

export function useUpdater(options: UseUpdaterOptions = {}): UseUpdaterResult {
  const checkOnMount = options.checkOnMount ?? true;
  const checkInterval = effectiveCheckInterval(options.checkInterval);
  const [state, setState] = useState<UpdaterState>(INITIAL_STATE);
  // Les options sont lues au moment de l'action : les actions restent stables d'un rendu à l'autre.
  const optionsRef = useRef(options);
  useEffect(() => {
    optionsRef.current = options;
  });
  // La mise à jour du plugin tient une ressource côté Rust, libérée dès qu'elle ne sert plus.
  const pending = useRef<Update | null>(null);
  const busy = useRef(false);
  const checkedOnMount = useRef(false);
  /** Heure de la dernière recherche tentée, réussie ou non ; l'intervalle court depuis elle. */
  const lastAttempt = useRef<number | null>(null);
  /**
   * Nombre de changements de canal enregistrés : une recherche ou une installation voit s'il a
   * changé pendant elle.
   */
  const channelChanges = useRef(0);
  /** Vrai une fois le composant démonté : une recherche encore en vol ne range ni ne relance rien. */
  const unmounted = useRef(false);

  const forget = useCallback(async () => {
    const update = pending.current;
    pending.current = null;
    await update?.close().catch(() => undefined);
  }, []);

  useEffect(() => {
    let active = true;
    void (async () => {
      try {
        const [currentVersion, settings] = await Promise.all([
          getVersion(),
          loadSettings(optionsRef.current.storage),
        ]);
        if (active) {
          setState((current) => ({ ...current, currentVersion, settings }));
        }
      } catch (error) {
        if (active) {
          setState((current) => ({
            ...current,
            phase: "error",
            error: `Réglages de mise à jour illisibles : ${describeUpdateError(error)}`,
          }));
        }
      }
    })();
    return () => {
      active = false;
    };
  }, []);

  useEffect(() => {
    unmounted.current = false;
    return () => {
      unmounted.current = true;
      void forget();
    };
  }, [forget]);

  const runCheck = useCallback(
    async (automatic: boolean) => {
      if (busy.current) {
        return;
      }
      busy.current = true;
      lastAttempt.current = Date.now();
      const channelAtStart = channelChanges.current;
      setState((current) => ({ ...current, phase: "checking", error: null, progress: null }));
      try {
        let update: Update | null;
        for (;;) {
          const changes = channelChanges.current;
          const { osVersion, timeout, storage } = optionsRef.current;
          update = await checkForUpdate({ osVersion, timeout, storage });
          if (unmounted.current) {
            // Plus personne pour la proposer : la réponse est libérée, sans autre recherche.
            await update?.close().catch(() => undefined);
            return;
          }
          if (changes === channelChanges.current) {
            break;
          }
          // Le canal a changé pendant la recherche : la réponse valait pour l'ancien. Elle est
          // libérée, et la recherche refaite avec le nouveau.
          await update?.close().catch(() => undefined);
        }
        // L'ancienne proposition n'est libérée qu'une fois la réponse reçue.
        await forget();
        pending.current = update;
        setState((current) => ({
          ...current,
          phase: update === null ? "upToDate" : "available",
          update: update === null ? null : describe(update),
          lastCheck: new Date(),
        }));
      } catch (error) {
        // Une version proposée avant un changement de canal venait de l'ancien : elle est oubliée,
        // comme le fait `setChannel` hors d'une recherche.
        if (channelChanges.current !== channelAtStart) {
          await forget();
        }
        // Sinon, une recherche qui échoue ne fait pas oublier une version déjà proposée, comme
        // dans le paquet Swift : automatique, elle se tait ; demandée, elle dit son erreur.
        const known = pending.current !== null;
        setState((current) =>
          automatic && known
            ? { ...current, phase: "available" }
            : {
                ...current,
                phase: "error",
                update: known ? current.update : null,
                error: `Recherche impossible : ${describeUpdateError(error)}`,
              },
        );
      } finally {
        busy.current = false;
      }
    },
    [forget],
  );

  const check = useCallback(() => runCheck(false), [runCheck]);

  const install = useCallback(async () => {
    const update = pending.current;
    if (update === null || busy.current) {
      return;
    }
    busy.current = true;
    const channelAtStart = channelChanges.current;
    setState((current) => ({
      ...current,
      phase: "downloading",
      error: null,
      progress: { downloaded: 0, total: null },
    }));
    try {
      await installUpdate(update, {
        downloadTimeout: optionsRef.current.downloadTimeout,
        onProgress: (progress) => {
          setState((current) => ({ ...current, progress }));
        },
        beforeInstall: async () => {
          setState((current) => ({ ...current, phase: "installing" }));
          await optionsRef.current.beforeInstall?.();
        },
      });
      // La relance est demandée : l'app se ferme dans l'instant, en restant sur « installing ».
    } catch (error) {
      // Une version retirée ne reviendra pas, et une version proposée avant un changement de
      // canal venait de l'ancien : on l'oublie. Après une autre erreur, la mise à jour reste
      // proposée, pour réessayer.
      const forgotten =
        isWithdrawnUpdateError(error) || channelChanges.current !== channelAtStart;
      if (forgotten) {
        await forget();
      }
      setState((current) => ({
        ...current,
        phase: "error",
        update: forgotten ? null : current.update,
        error: `Installation impossible : ${describeUpdateError(error)}`,
      }));
    } finally {
      busy.current = false;
    }
  }, [forget]);

  const setChannel = useCallback(
    async (channel: UpdateChannel) => {
      try {
        const settings = await saveSettings({ channel }, optionsRef.current.storage);
        channelChanges.current += 1;
        if (busy.current) {
          // Une recherche en cours se refait seule avec ce canal ; une installation va à son terme.
          setState((current) => ({ ...current, settings }));
          return;
        }
        // La mise à jour affichée, ou l'erreur, venait peut-être de l'autre canal.
        await forget();
        setState((current) => ({ ...current, settings, update: null, phase: "idle", error: null }));
      } catch (error) {
        setState((current) => ({
          ...current,
          phase: busy.current ? current.phase : "error",
          error: `Réglage non enregistré : ${describeUpdateError(error)}`,
        }));
      }
    },
    [forget],
  );

  const setShareStatistics = useCallback(async (enabled: boolean) => {
    try {
      const settings = await saveSettings(
        { shareStatistics: enabled },
        optionsRef.current.storage,
      );
      setState((current) => ({ ...current, settings }));
    } catch (error) {
      setState((current) => ({
        ...current,
        phase: busy.current ? current.phase : "error",
        error: `Réglage non enregistré : ${describeUpdateError(error)}`,
      }));
    }
  }, []);

  useEffect(() => {
    if (checkOnMount && !checkedOnMount.current) {
      checkedOnMount.current = true;
      void check();
    }
  }, [checkOnMount, check]);

  // Recherche périodique : une app laissée ouverte finit par voir les nouvelles versions.
  useEffect(() => {
    if (checkInterval === null) {
      return;
    }
    const mounted = Date.now();
    let timer: ReturnType<typeof setTimeout> | undefined;
    const due = () => {
      // Une heure future (horloge reculée) ne repousse pas au-delà d'un intervalle : l'attente
      // repart de maintenant.
      if ((lastAttempt.current ?? mounted) > Date.now()) {
        lastAttempt.current = Date.now();
      }
      return (lastAttempt.current ?? mounted) + checkInterval;
    };
    const schedule = () => {
      // Réveil dans une heure au plus, et l'heure est relue à chaque fois : un minuteur ne compte
      // pas toujours le temps passé en veille, et ne sait pas attendre plus de 24 jours.
      const delay = Math.min(Math.max(due() - Date.now(), 0), HOUR);
      timer = setTimeout(() => {
        if (Date.now() >= due()) {
          // Comptée comme une tentative même si elle est écartée (installation en cours) : la
          // suivante attend un intervalle entier.
          lastAttempt.current = Date.now();
          void runCheck(true);
        }
        schedule();
      }, delay);
    };
    schedule();
    return () => {
      clearTimeout(timer);
    };
  }, [checkInterval, runCheck]);

  return { ...state, check, install, setChannel, setShareStatistics };
}

function describe(update: Update): AvailableUpdate {
  return {
    version: update.version,
    currentVersion: update.currentVersion,
    notes: update.body ?? "",
    date: update.date ?? null,
    channel: updateChannel(update),
  };
}
