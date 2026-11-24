/**
 * Mode d'essai de bout en bout (voir README.md) : options lues par Rust sur la ligne de commande,
 * et journal des étapes. Hors de ce mode, le journal ne fait rien. Une vraie app n'en a pas besoin.
 */
import type { UpdateChannel } from "@drakbase/tauri";
import { invoke } from "@tauri-apps/api/core";

export interface LaunchOptions {
  /** Vrai quand l'app a été lancée avec `--journal`. */
  e2e: boolean;
  /** Cherche, installe et relance sans intervention. */
  autoUpdate: boolean;
  channel: UpdateChannel | null;
  shareStatistics: boolean | null;
}

export function launchOptions(): Promise<LaunchOptions> {
  return invoke<LaunchOptions>("launch_options");
}

export function journal(event: string, data: Record<string, unknown> = {}): void {
  invoke("journal", { event, data }).catch(() => undefined);
}
