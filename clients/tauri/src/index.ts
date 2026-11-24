/**
 * Aide des apps Tauri 2 pour les mises à jour de DrakBase - Apps Market. Le crochet React est dans
 * `@drakbase/tauri/react`, pour qu'une app sans React n'ait pas à l'installer.
 */
export {
  checkForUpdate,
  describeUpdateError,
  installUpdate,
  isWithdrawnUpdateError,
  updateChannel,
  type CheckForUpdateOptions,
  type DownloadProgress,
  type InstallUpdateOptions,
} from "./updater";
export {
  installId,
  loadSettings,
  saveSettings,
  type SettingsStorage,
  type UpdateChannel,
  type UpdaterSettings,
} from "./settings";
