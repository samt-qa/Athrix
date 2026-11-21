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
