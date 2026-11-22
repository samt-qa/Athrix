import { createSignal } from "solid-js";

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

/**
 * An on/off setting kept in local storage as "1" / "0", followed across windows. Returns the
 * reactive getter and a setter that persists. Create it at module level (one per key).
 */
export function storedFlag(key: string, fallback: boolean): [() => boolean, (on: boolean) => void] {
  const read = (v: string | null) => (v === "1" ? true : v === "0" ? false : fallback);
  const [get, set] = createSignal(read(readSetting(key)));
  onSettingChange(key, (v) => set(read(v)));
  return [get, (on: boolean) => {
    set(on);
    writeSetting(key, on ? "1" : "0");
  }];
}

/**
 * A capped set of strings kept in local storage as a JSON array (oldest first), followed
 * across windows. Adding past `cap` forgets the oldest entries; a corrupt or missing value reads
 * as empty. Like the other helpers it never throws: without storage it only lasts the session.
 */
export function storedSet(key: string, cap: number): { has: (value: string) => boolean; add: (value: string) => void } {
  const parse = (raw: string | null): Set<string> => {
    if (!raw) return new Set();
    try {
      const list: unknown = JSON.parse(raw);
      return new Set(Array.isArray(list) ? list.filter((v): v is string => typeof v === "string").slice(-cap) : []);
    } catch {
      return new Set();
    }
  };
  let items = parse(readSetting(key));
  onSettingChange(key, (v) => {
    items = parse(v);
  });
  return {
    has: (value) => items.has(value),
    add(value) {
      // Re-adding moves it to the newest end, so the most recently dismissed survive the cap.
      items.delete(value);
      items.add(value);
      for (const oldest of items) {
        if (items.size <= cap) break;
        items.delete(oldest);
      }
      writeSetting(key, JSON.stringify([...items]));
    },
  };
}
