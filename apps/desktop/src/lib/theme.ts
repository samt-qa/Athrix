import { createSignal } from "solid-js";
import { readSetting, writeSetting } from "./storage";

export const THEMES = ["system", "light", "dark"] as const;
export type Theme = (typeof THEMES)[number];

const isTheme = (v: unknown): v is Theme => THEMES.includes(v as Theme);

/** "system" removes data-theme so tokens.css follows prefers-color-scheme. */
function applyTheme(theme: Theme) {
  const root = document.documentElement;
  if (theme === "system") root.removeAttribute("data-theme");
  else root.setAttribute("data-theme", theme);
}

const saved = readSetting("theme");
const [theme, setThemeSignal] = createSignal<Theme>(isTheme(saved) ? saved : "system");
applyTheme(theme()); // at import time, before the first render

export { theme };

export function setTheme(next: Theme) {
  setThemeSignal(next);
  applyTheme(next);
  writeSetting("theme", next);
}
