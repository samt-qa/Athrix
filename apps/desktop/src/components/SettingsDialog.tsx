import { createSignal, For, Match, onCleanup, onMount, Show, Switch, type JSX } from "solid-js";
import { locale, t, type MessageKey } from "../i18n";
import { api, errorMessage, onContentStatus, usingMock, type AppInfo, type ContentStatus, type DriveSettings } from "../lib/api";
import { formatCount } from "../lib/format";
import { clearHistory, historyEnabled, setHistoryEnabled } from "../lib/library";
import { searchContent, setSearchContent } from "../lib/searchContent";
import DrivePicker from "./IndexSettings";
import { autoInterpret, setAutoInterpret } from "./InterpretHint";
import Modal from "./Modal";
import UpdateSection from "./UpdateSection";
import McpSection from "./McpSection";
import { LanguageSetting, ThemeSetting } from "./SettingsControls";

export type SettingsSection = "general" | "search" | "indexing" | "shortcuts" | "mcp" | "about";
const SECTIONS: SettingsSection[] = ["general", "search", "indexing", "shortcuts", "mcp", "about"];

type Props = {
  info: AppInfo | null | undefined;
  initial?: SettingsSection;
  onClose: () => void;
  /** "Show the welcome screen again". */
  onShowWelcome: () => void;
  /** A transient message for the status bar. */
  onNotice: (message: string) => void;
};

/** An on/off row: a switch-styled checkbox with a label and an optional hint. */
export function SwitchRow(props: {
  checked: boolean;
  onChange: (on: boolean) => void;
  label: string;
  hint?: string;
  disabled?: boolean;
}) {
  return (
    <label class="switch-row" classList={{ disabled: props.disabled }}>
      <span class="switch-text">
        <span class="switch-label">{props.label}</span>
        <Show when={props.hint}><span class="switch-hint">{props.hint}</span></Show>
      </span>
      <input
        type="checkbox"
        role="switch"
        class="switch"
        checked={props.checked}
        disabled={props.disabled}
        onChange={(e) => props.onChange(e.currentTarget.checked)}
      />
    </label>
  );
}

function Section(props: { id: string; title: string; children: JSX.Element }) {
  return (
    <section class="settings-group" aria-labelledby={props.id}>
      <h3 id={props.id} class="settings-group-title">{props.title}</h3>
      {props.children}
    </section>
  );
}

/** Keyboard shortcuts, read-only for now (design §12). */
const SHORTCUTS: { keys: string[]; label: MessageKey }[] = [
  { keys: ["Ctrl", "Shift", "Space"], label: "shortcuts.quickSearch" },
  { keys: ["↑", "↓"], label: "shortcuts.navigate" },
  { keys: ["Enter"], label: "shortcuts.open" },
  { keys: ["Ctrl", "Enter"], label: "shortcuts.reveal" },
  { keys: ["Ctrl", "C"], label: "shortcuts.copyPath" },
  { keys: ["Ctrl", "D"], label: "shortcuts.favorite" },
  { keys: ["Ctrl", "S"], label: "shortcuts.saveSearch" },
  { keys: ["Shift", "F10"], label: "shortcuts.menu" },
  { keys: ["F3"], label: "shortcuts.nextMatch" },
  { keys: ["Ctrl", ","], label: "shortcuts.settings" },
  { keys: ["Esc"], label: "shortcuts.escape" },
];

/**
 * The settings dialog (task 5.2): vertical tabs (design §9.3) for general, search, indexing,
 * shortcuts and about. Every change applies at once; there is no "save" for the whole dialog
 * (only the drive choice, which re-indexes, has its own button).
 */
export default function SettingsDialog(props: Props) {
  const [section, setSection] = createSignal<SettingsSection>(props.initial ?? "general");
  const tabs: HTMLButtonElement[] = [];
  const tabId = (s: SettingsSection) => `settings-tab-${s}`;
  const panelId = (s: SettingsSection) => `settings-panel-${s}`;

  function onTabKey(e: KeyboardEvent, i: number) {
    const n = SECTIONS.length;
    let next: number;
    switch (e.key) {
      case "ArrowDown": next = (i + 1) % n; break;
      case "ArrowUp": next = (i - 1 + n) % n; break;
      case "Home": next = 0; break;
      case "End": next = n - 1; break;
      default: return;
    }
    e.preventDefault();
    setSection(SECTIONS[next]);
    tabs[next]?.focus();
  }

  // ---- external drives in "the whole computer" (task 2.5) ----
  const [drives, setDrives] = createSignal<DriveSettings | null>(null);
  const [drivesBusy, setDrivesBusy] = createSignal(false);
  onMount(() => {
    api.getDriveSettings().then(setDrives, (err) => console.warn("drive settings unavailable", err));
  });
  async function changeDrives(change: Partial<DriveSettings>) {
    const current = drives();
    if (!current) return;
    setDrivesBusy(true);
    try {
      setDrives(await api.setDriveSettings({ ...current, ...change }));
    } catch (err) {
      props.onNotice(t("drives.failed", { message: errorMessage(err) }));
    } finally {
      setDrivesBusy(false);
    }
  }

  // ---- content indexing (live while the dialog is open) ----
  const [content, setContent] = createSignal<ContentStatus | null>(null);
  const [contentBusy, setContentBusy] = createSignal(false);
  onMount(async () => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    onCleanup(() => {
      disposed = true;
      unlisten?.();
    });
    try {
      unlisten = await onContentStatus(setContent);
      if (disposed) unlisten();
      setContent(await api.contentStatus());
    } catch (err) {
      console.warn("content status unavailable", err);
    }
  });
  async function contentAction(run: () => Promise<void>) {
    setContentBusy(true);
    try {
      await run();
      setContent(await api.contentStatus());
    } catch (err) {
      props.onNotice(t("content.actionFailed", { message: errorMessage(err) }));
    } finally {
      setContentBusy(false);
    }
  }
  const contentLine = () => {
    const s = content();
    if (!s) return "";
    const n = (v: number) => formatCount(v, locale());
    switch (s.state) {
      case "disabled": return t("content.disabled");
      case "running": return t("content.progress", { processed: n(s.processed), total: n(s.total) });
      case "paused": {
        const sep = locale() === "ar" ? "، " : ", ";
        const reasons = (s.pausedBy.length ? s.pausedBy : ["user" as const]).map((r) => t(`content.reason.${r}` as MessageKey));
        return t("content.paused", { reasons: reasons.join(sep) });
      }
      case "error": return s.error ? t("content.errorDetail", { message: s.error }) : t("content.error");
      case "stopped": return t("content.stopped");
      default: return t("settings.contentIndexed", { count: n(s.indexed) });
    }
  };

  // ---- history: "clear" asks once more inline ----
  const [confirmClear, setConfirmClear] = createSignal(false);
  async function doClear() {
    try {
      await clearHistory();
      props.onNotice(t("settings.historyCleared"));
    } catch (err) {
      props.onNotice(t("library.actionFailed", { message: errorMessage(err) }));
    } finally {
      setConfirmClear(false);
    }
  }

  return (
    <Modal labelledBy="settings-title" onClose={props.onClose} class="settings" initialFocus={() => tabs[SECTIONS.indexOf(section())]}>
      <header class="settings-head">
        <h2 id="settings-title" class="modal-title">{t("settings.title")}</h2>
        <button type="button" class="icon-btn" aria-label={t("dialog.close")} title={t("dialog.close")} onClick={props.onClose}>
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75"
            stroke-linecap="round" aria-hidden="true"><path d="M6 6l12 12M18 6 6 18" /></svg>
        </button>
      </header>
      <div class="settings-body">
        <div class="settings-tabs" role="tablist" aria-orientation="vertical" aria-label={t("settings.sections")}>
          <For each={SECTIONS}>
            {(s, i) => (
              <button
                ref={(el) => (tabs[i()] = el)}
                type="button"
                role="tab"
                id={tabId(s)}
                class="settings-tab"
                aria-selected={section() === s}
                aria-controls={panelId(s)}
                tabIndex={section() === s ? 0 : -1}
                onClick={() => setSection(s)}
                onKeyDown={(e) => onTabKey(e, i())}
              >
                {t(`settings.section.${s}` as MessageKey)}
              </button>
            )}
          </For>
        </div>

        <div class="settings-panel" role="tabpanel" id={panelId(section())} aria-labelledby={tabId(section())} tabIndex={0}>
          <Switch>
            <Match when={section() === "general"}>
              <Section id="set-lang" title={t("settings.language")}>
                <LanguageSetting />
              </Section>
              <Section id="set-theme" title={t("settings.theme")}>
                <ThemeSetting />
              </Section>
              <Section id="set-welcome" title={t("settings.welcome")}>
                <p class="settings-hint">{t("settings.welcomeHint")}</p>
                <button type="button" class="btn-secondary" onClick={props.onShowWelcome}>{t("settings.welcomeShow")}</button>
              </Section>
            </Match>

            <Match when={section() === "search"}>
              <Section id="set-content-search" title={t("settings.searchWhat")}>
                <SwitchRow
                  checked={searchContent()}
                  onChange={setSearchContent}
                  label={t("content.searchToo")}
                  hint={t("content.searchTooHint")}
                />
                <SwitchRow
                  checked={autoInterpret()}
                  onChange={setAutoInterpret}
                  label={t("settings.interpretAuto")}
                  hint={t("settings.interpretAutoHint")}
                />
              </Section>
              <Section id="set-history" title={t("settings.history")}>
                <SwitchRow
                  checked={historyEnabled()}
                  onChange={setHistoryEnabled}
                  label={t("settings.historyKeep")}
                  hint={t("settings.historyHint")}
                />
                <div class="settings-inline">
                  <Show
                    when={confirmClear()}
                    fallback={
                      <button type="button" class="btn-secondary" onClick={() => setConfirmClear(true)}>
                        {t("settings.historyClear")}
                      </button>
                    }
                  >
                    <span class="settings-hint" role="alert">{t("settings.historyClearConfirm")}</span>
                    <button type="button" class="btn-danger" ref={(el) => queueMicrotask(() => el.focus())} onClick={() => void doClear()}>
                      {t("settings.historyClearYes")}
                    </button>
                    <button type="button" class="btn-secondary" onClick={() => setConfirmClear(false)}>{t("dialog.cancel")}</button>
                  </Show>
                </div>
              </Section>
            </Match>

            <Match when={section() === "indexing"}>
              <Section id="set-drives" title={t("drives.title")}>
                <DrivePicker
                  mode="settings"
                  labelledBy="set-drives"
                  onError={(message) => props.onNotice(t("drives.failed", { message }))}
                />
              </Section>
              <Section id="set-external" title={t("drives.externalTitle")}>
                <Show when={drives()} fallback={<p class="settings-hint">{t("drives.loading")}</p>}>
                  {(d) => (
                    <>
                      <SwitchRow
                        checked={d().includeRemovable}
                        disabled={drivesBusy()}
                        onChange={(on) => void changeDrives({ includeRemovable: on })}
                        label={t("drives.includeRemovable")}
                        hint={t("drives.includeRemovableHint")}
                      />
                      <SwitchRow
                        checked={d().includeNetwork}
                        disabled={drivesBusy()}
                        onChange={(on) => void changeDrives({ includeNetwork: on })}
                        label={t("drives.includeNetwork")}
                        hint={t("drives.includeNetworkHint")}
                      />
                      <SwitchRow
                        checked={d().includeOnedrive}
                        disabled={drivesBusy()}
                        onChange={(on) => void changeDrives({ includeOnedrive: on })}
                        label={t("drives.includeOnedrive")}
                        hint={t("drives.includeOnedriveHint")}
                      />
                    </>
                  )}
                </Show>
              </Section>
              <Section id="set-content" title={t("content.title")}>
                <Show when={content()} fallback={<p class="settings-hint">{t("settings.contentUnavailable")}</p>}>
                  {(s) => (
                    <>
                      <SwitchRow
                        checked={s().state !== "disabled"}
                        disabled={contentBusy()}
                        onChange={(on) => void contentAction(() => api.setContentIndexing(on))}
                        label={t("content.enable")}
                        hint={t("settings.contentHint")}
                      />
                      <div class="settings-inline">
                        <span class="settings-status" role="status">{contentLine()}</span>
                        <Show when={s().state === "running"}>
                          <button type="button" class="btn-secondary" disabled={contentBusy()}
                            onClick={() => void contentAction(() => api.contentPause())}>
                            {t("content.pause")}
                          </button>
                        </Show>
                        <Show when={s().state === "paused" && s().pausedBy.includes("user")}>
                          <button type="button" class="btn-secondary" disabled={contentBusy()}
                            onClick={() => void contentAction(() => api.contentResume())}>
                            {t("content.resume")}
                          </button>
                        </Show>
                      </div>
                    </>
                  )}
                </Show>
              </Section>
            </Match>

            <Match when={section() === "shortcuts"}>
              <Section id="set-keys" title={t("settings.section.shortcuts")}>
                <p class="settings-hint">{t("settings.shortcutsHint")}</p>
                <dl class="shortcut-list">
                  <For each={SHORTCUTS}>
                    {(s) => (
                      <div class="shortcut-row">
                        <dt>{t(s.label)}</dt>
                        <dd dir="ltr">
                          <For each={s.keys}>{(k) => <kbd>{k}</kbd>}</For>
                        </dd>
                      </div>
                    )}
                  </For>
                </dl>
              </Section>
            </Match>

            <Match when={section() === "mcp"}>
              <McpSection onNotice={props.onNotice} />
            </Match>

            <Match when={section() === "about"}>
              <div class="about">
                <img src="/athrix.svg" alt="" width="48" height="48" />
                <p class="about-name"><span lang="en">{t("app.brandName")}</span> · <span lang="ar">{t("app.brandNative")}</span></p>
                <p class="settings-hint">{t("about.tagline")}</p>
                <p class="about-version">
                  <Show when={props.info} fallback={t("about.versionUnknown")}>
                    {(i) => <>{t("about.version")} <bdi dir="ltr">{i().version}</bdi></>}
                  </Show>
                </p>
                <Show when={usingMock}>
                  <p class="settings-hint">{t("about.mock")}</p>
                </Show>
              </div>
              <UpdateSection onNotice={props.onNotice} />
            </Match>
          </Switch>
        </div>
      </div>
    </Modal>
  );
}
