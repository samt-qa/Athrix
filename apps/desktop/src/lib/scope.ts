// The search scope: "" = the whole computer, otherwise one indexed root (e.g. "D:\\").
// Remembered in local storage and shared by the main window and the quick-search popup.
import { createSignal } from "solid-js";
import { onSettingChange, readSetting, writeSetting } from "./storage";

const KEY = "searchScope";

const [storedScope, setStoredScope] = createSignal<string>(readSetting(KEY) ?? "");
onSettingChange(KEY, (v) => setStoredScope(v ?? ""));

/** The remembered scope, as chosen (may name a root that is no longer indexed). */
export { storedScope };

export function setScope(scope: string): void {
  setStoredScope(scope);
  writeSetting(KEY, scope);
}

/** Comparable form of a root path: "/" separators, no trailing separator, lower case. */
export function rootKey(path: string): string {
  return path.replace(/\\/g, "/").replace(/\/+$/, "").toLowerCase();
}

/**
 * The scope to send with a search: the stored one if it is among `roots` (returned with the
 * spelling of `roots`), else "" (everywhere). Unknown roots (index not loaded yet) keep it.
 */
export function effectiveScope(stored: string, roots: string[] | undefined): string {
  if (!stored) return "";
  if (!roots || roots.length === 0) return stored;
  return roots.find((r) => rootKey(r) === rootKey(stored)) ?? "";
}

/** Short name of a root for the UI: "C:" for "C:\\", the last folder name otherwise. */
export function rootName(path: string): string {
  if (/^[a-z]:[\\/]?$/i.test(path)) return path.slice(0, 2).toUpperCase();
  const trimmed = path.replace(/[\\/]+$/, "");
  if (!trimmed) return "/";
  const parts = trimmed.split(/[\\/]/);
  return parts[parts.length - 1] || trimmed;
}
