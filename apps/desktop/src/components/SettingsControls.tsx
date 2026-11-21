import type { JSX } from "solid-js";
import Segmented from "./Segmented";
import { locale, setLocale, t, type Locale } from "../i18n";
import { theme, setTheme, type Theme } from "../lib/theme";

const iconProps = {
  width: 16, height: 16, viewBox: "0 0 24 24", fill: "none", stroke: "currentColor",
  "stroke-width": 1.75, "stroke-linecap": "round", "stroke-linejoin": "round", "aria-hidden": true,
} as const;

const SystemIcon = () => (
  <svg {...iconProps}><rect x="3" y="4" width="18" height="12" rx="2" /><path d="M8 20h8M12 16v4" /></svg>
);
const SunIcon = () => (
  <svg {...iconProps}>
    <circle cx="12" cy="12" r="4" />
    <path d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4" />
  </svg>
);
const MoonIcon = () => (
  <svg {...iconProps}><path d="M20 14.5A8 8 0 0 1 9.5 4a8 8 0 1 0 10.5 10.5Z" /></svg>
);

/** Icon + word, for the labelled settings rows. */
const Labeled = (props: { icon?: JSX.Element; text: string; lang?: string }) => (
  <span class="seg-labeled" lang={props.lang}>
    {props.icon}
    <span>{props.text}</span>
  </span>
);

/** UI language: Arabic / English (applies at once, in both windows). */
export function LanguageSetting() {
  return (
    <Segmented<Locale>
      label={t("settings.language")}
      value={locale()}
      onChange={setLocale}
      options={[
        { value: "ar", label: t("settings.languageAr"), content: <Labeled text="العربية" lang="ar" />, lang: "ar" },
        { value: "en", label: t("settings.languageEn"), content: <Labeled text="English" lang="en" />, lang: "en" },
      ]}
    />
  );
}

/** Theme: follow the system / light / dark. */
export function ThemeSetting() {
  return (
    <Segmented<Theme>
      label={t("settings.theme")}
      value={theme()}
      onChange={setTheme}
      options={[
        { value: "system", label: t("settings.themeSystem"), content: <Labeled icon={<SystemIcon />} text={t("settings.themeSystem")} /> },
        { value: "light", label: t("settings.themeLight"), content: <Labeled icon={<SunIcon />} text={t("settings.themeLight")} /> },
        { value: "dark", label: t("settings.themeDark"), content: <Labeled icon={<MoonIcon />} text={t("settings.themeDark")} /> },
      ]}
    />
  );
}
