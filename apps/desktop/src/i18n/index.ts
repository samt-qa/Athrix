import { createSignal } from "solid-js";
import ar from "./ar.json";
import en from "./en.json";
import { readSetting, writeSetting } from "../lib/storage";

/** Arabic is the source of truth: every key must exist in ar.json. */
export type MessageKey = keyof typeof ar;
type Dictionary = Record<MessageKey, string>;

// en.json must have exactly the keys of ar.json (missing key = error here, extra key = error below).
const dictionaries: Record<Locale, Dictionary> = { ar, en };
export type AssertNoExtraKeys = Exclude<keyof typeof en, MessageKey> extends never ? true : never;
export const assertNoExtraKeys: AssertNoExtraKeys = true;

export const LOCALES = ["ar", "en"] as const;
export type Locale = (typeof LOCALES)[number];
export const DEFAULT_LOCALE: Locale = "ar";

const isLocale = (v: unknown): v is Locale => LOCALES.includes(v as Locale);
export const localeDir = (l: Locale): "rtl" | "ltr" => (l === "ar" ? "rtl" : "ltr");

function applyLocale(l: Locale) {
  const root = document.documentElement;
  root.lang = l;
  root.dir = localeDir(l);
}

const saved = readSetting("locale");
const [locale, setLocaleSignal] = createSignal<Locale>(isLocale(saved) ? saved : DEFAULT_LOCALE);
applyLocale(locale()); // at import time, before the first render

export { locale };

export function setLocale(l: Locale) {
  setLocaleSignal(l);
  applyLocale(l);
  writeSetting("locale", l);
}

export type MessageParams = Record<string, string | number>;

/** Translates `key` for the current locale (reactive). `{name}` placeholders are filled from `params`. */
export function t(key: MessageKey, params?: MessageParams): string {
  const text = dictionaries[locale()][key] ?? dictionaries[DEFAULT_LOCALE][key] ?? key;
  if (!params) return text;
  return text.replace(/\{(\w+)\}/g, (m, name: string) => (name in params ? String(params[name]) : m));
}
