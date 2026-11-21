import { createResource, createSignal, Show } from "solid-js";
import { isTauri } from "@tauri-apps/api/core";
import { t } from "./i18n";
import { safeInvoke } from "./lib/tauri";
import SettingsControls from "./components/SettingsControls";

type AppInfo = { name: string; version: string };

function SearchIcon() {
  return (
    <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor"
      stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <circle cx="11" cy="11" r="7" />
      <path d="m20 20-3.5-3.5" />
    </svg>
  );
}

export default function App() {
  const inTauri = isTauri();
  const [info] = createResource(() => safeInvoke<AppInfo>("app_info"));
  const [query, setQuery] = createSignal("");
  // Temporary demo of the UI-to-Rust bridge: how the name index will store the text (task 1.6 replaces it).
  const [normalized] = createResource(query, (q) => safeInvoke<string>("normalize_preview", { text: q }));

  return (
    <div class="shell">
      <header class="topbar">
        <div class="brand">
          <img src="/athrix.svg" alt="" />
          <span class="brand-name" lang="en">{t("app.brandName")}</span>
          <span class="brand-ar" lang="ar">{t("app.brandNative")}</span>
        </div>
        <Show when={info()}>
          {(i) => (
            <bdi class="version" dir="ltr" title={t("topbar.versionTitle")}>
              {t("topbar.version", { version: i().version })}
            </bdi>
          )}
        </Show>
        <SettingsControls />
      </header>

      <main class="main">
        <section class="panel" aria-label={t("search.region")}>
          <label class="search">
            <SearchIcon />
            <input
              dir="auto"
              autofocus
              aria-label={t("search.inputLabel")}
              placeholder={t("search.placeholder")}
              value={query()}
              onInput={(e) => setQuery(e.currentTarget.value)}
              onKeyDown={(e) => e.key === "Escape" && setQuery("")}
            />
          </label>
          <div class="progress" />
          <div class="preview">
            <span class="label">{t("preview.label")}</span>
            <bdi class="normalized" dir="auto">
              {!query()
                ? <span class="empty">{t("preview.empty")}</span>
                : inTauri
                  ? normalized() ?? ""
                  : <span class="empty">{t("preview.unavailable")}</span>}
            </bdi>
            <span class="hint">{t("preview.hint")}</span>
          </div>
        </section>
      </main>

      <footer class="statusbar">
        <span>{t("status.index")}</span>
        <span><kbd>Esc</kbd> {t("status.clear")}</span>
      </footer>
    </div>
  );
}
