import { createSignal, Match, onMount, Show, Switch } from "solid-js";
import { t } from "../i18n";
import { api, errorMessage, type UpdateInfo } from "../lib/api";
import { SwitchRow } from "./SettingsDialog";

type Check =
  | { kind: "idle" }
  | { kind: "checking" }
  | { kind: "done"; info: UpdateInfo }
  | { kind: "failed"; message: string };

/**
 * Settings → About: "check for updates", the result, install (after a confirm), and the
 * "check automatically at start-up" switch (off by default; the only time Athrix goes online).
 */
export default function UpdateSection(props: { onNotice: (message: string) => void }) {
  const [check, setCheck] = createSignal<Check>({ kind: "idle" });
  const [auto, setAuto] = createSignal<boolean | null>(null);
  const [confirmInstall, setConfirmInstall] = createSignal(false);
  const [installing, setInstalling] = createSignal(false);

  onMount(() => {
    api.getUpdateSettings().then((s) => setAuto(s.autoUpdateCheck)).catch(() => setAuto(null));
  });

  async function runCheck() {
    setConfirmInstall(false);
    setCheck({ kind: "checking" });
    try {
      setCheck({ kind: "done", info: await api.updateCheck() });
    } catch (err) {
      setCheck({ kind: "failed", message: errorMessage(err) });
    }
  }

  async function setAutoCheck(on: boolean) {
    const before = auto();
    setAuto(on);
    try {
      setAuto((await api.setUpdateSettings({ autoUpdateCheck: on })).autoUpdateCheck);
    } catch (err) {
      setAuto(before);
      props.onNotice(t("update.settingFailed", { message: errorMessage(err) }));
    }
  }

  async function install() {
    setInstalling(true);
    try {
      await api.updateInstall(); // restarts the app on success
    } catch (err) {
      setCheck({ kind: "failed", message: errorMessage(err) });
      setConfirmInstall(false);
    } finally {
      setInstalling(false);
    }
  }

  const available = () => {
    const c = check();
    return c.kind === "done" && c.info.available ? c.info : null;
  };

  const failure = () => {
    const c = check();
    return c.kind === "failed" ? c.message : null;
  };

  return (
    <section class="settings-group" aria-labelledby="set-update">
      <h3 id="set-update" class="settings-group-title">{t("update.title")}</h3>
      <div class="settings-inline">
        <button type="button" class="btn-secondary" disabled={check().kind === "checking" || installing()} onClick={() => void runCheck()}>
          {t("update.check")}
        </button>
        <span class="settings-status" role="status">
          <Switch>
            <Match when={check().kind === "checking"}>{t("update.checking")}</Match>
            <Match when={check().kind === "done" && !available()}>{t("update.upToDate")}</Match>
            <Match when={available()}>
              {(info) => <>{t("update.available")} <bdi dir="ltr">{info().version ?? ""}</bdi></>}
            </Match>
            <Match when={failure()}>
              {(m) => (
                <span class="update-failed">
                  {t("update.failed")} <bdi dir="ltr" class="update-detail">{m()}</bdi>
                </span>
              )}
            </Match>
          </Switch>
        </span>
      </div>

      <Show when={available()}>
        {(info) => (
          <>
            <Show when={info().notes}>
              <p class="update-notes" dir="auto">{info().notes}</p>
            </Show>
            <div class="settings-inline">
              <Show
                when={confirmInstall()}
                fallback={<button type="button" class="btn-primary" onClick={() => setConfirmInstall(true)}>{t("update.install")}</button>}
              >
                <span class="settings-hint" role="alert">{t("update.installConfirm")}</span>
                <button type="button" class="btn-primary" disabled={installing()} ref={(el) => queueMicrotask(() => el.focus())}
                  onClick={() => void install()}>
                  {installing() ? t("update.installing") : t("update.installYes")}
                </button>
                <button type="button" class="btn-secondary" disabled={installing()} onClick={() => setConfirmInstall(false)}>
                  {t("dialog.cancel")}
                </button>
              </Show>
            </div>
          </>
        )}
      </Show>

      <Show when={auto() !== null}>
        <SwitchRow checked={!!auto()} onChange={(on) => void setAutoCheck(on)} label={t("update.auto")} hint={t("update.autoHint")} />
      </Show>
    </section>
  );
}
