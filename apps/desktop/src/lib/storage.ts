// localStorage access that never throws: storage can be disabled, full, or blocked.
const PREFIX = "athrix.";

export function readSetting(key: string): string | null {
  try {
    return window.localStorage.getItem(PREFIX + key);
  } catch {
    return null;
  }
}

export function writeSetting(key: string, value: string): void {
  try {
    window.localStorage.setItem(PREFIX + key, value);
  } catch {
    // Ignore: the setting just won't survive a restart.
  }
}

/**
 * Calls `handler` when another window of the app (main window or quick-search popup) changes
 * `key`. Same-origin windows share local storage; the `storage` event only fires in the others.
 */
export function onSettingChange(key: string, handler: (value: string | null) => void): void {
  window.addEventListener("storage", (e) => {
    if (e.key === PREFIX + key) handler(e.newValue);
  });
}
