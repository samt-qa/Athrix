import { createSignal, Match, onCleanup, onMount, Show, Switch } from "solid-js";
import { locale, t, tPlural, type MessageKey } from "../i18n";
import { api, errorMessage, onContentStatus, type ContentPauseReason, type ContentStatus } from "../lib/api";
import { formatCount } from "../lib/format";

type Props = {
  /** Called with a message when a command fails. */
  onError: (message: string) => void;
  /** Opens Settings → Indexing (content indexing on/off and "search contents too" live there). */
  onOpenSettings?: () => void;
};

const PANEL_ID = "content-panel";

/** Localized pause reasons: "on battery", "paused by you"… */
function reasons(s: ContentStatus): string {
  const sep = locale() === "ar" ? "، " : ", ";
  const list = s.pausedBy.length ? s.pausedBy : ["user" as const];
  return list.map((r) => t(`content.reason.${r}` as MessageKey)).join(sep);
}

/** The reason that decides when a pause ends: the user's own pause wins (only they can lift it). */
function pauseCause(s: ContentStatus): ContentPauseReason {
  if (!s.pausedBy.length || s.pausedBy.includes("user")) return "user";
  return s.pausedBy[0];
}

const showsProgress = (s: ContentStatus) => (s.state === "running" || s.state === "paused") && s.total > 0;

/**
 * Content indexing in the status bar (task 4.5): one compact line with a thin progress bar
 * ("فهرسة المحتوى: 12,340 / 80,000", "متوقف مؤقتًا: على البطارية"). A click opens a small
 * panel above it: progress and counts, pause / resume, the "index file contents" switch
 * (`set_content_indexing`) and the "search file contents too" search setting. Esc or a click
 * outside closes it. Hidden when the backend has no content status (older backend).
 */
export default function ContentStatusControl(props: Props) {
  const [status, setStatus] = createSignal<ContentStatus | null>(null);
  const [open, setOpen] = createSignal(false);
  const [busy, setBusy] = createSignal(false);
  let wrapper!: HTMLDivElement;
  let button: HTMLButtonElement | undefined;
  let panel: HTMLDivElement | undefined;

  // Subscribe first, then read the current state (an event may arrive in between).
  onMount(async () => {
    let gotEvent = false;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    onCleanup(() => {
      disposed = true;
      unlisten?.();
    });
    try {
      unlisten = await onContentStatus((s) => {
        gotEvent = true;
        setStatus(s);
      });
      if (disposed) unlisten();
      const s = await api.contentStatus();
      if (!gotEvent) setStatus(s);
    } catch (err) {
      console.warn("content status unavailable", err);
    }
  });

  const refresh = () => api.contentStatus().then(setStatus).catch(() => {});

  async function run(action: () => Promise<void>) {
    setBusy(true);
    try {
      await action();
      await refresh();
    } catch (err) {
      props.onError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  }

  const userPaused = () => status()?.state === "paused" && status()!.pausedBy.includes("user");
  const canPause = () => {
    const s = status()?.state;
    return s === "running" || s === "idle" || (s === "paused" && !userPaused());
  };

  /** The one-line summary (button text). */
  const summary = (s: ContentStatus): string => {
    const n = (x: number) => formatCount(x, locale());
    switch (s.state) {
      case "running": return t("content.progress", { processed: n(s.processed), total: n(s.total) });
      case "paused": return t("content.paused", { reasons: reasons(s) });
      case "idle": return tPlural("content.ready", s.indexed, n(s.indexed));
      case "disabled": return t("content.disabled");
      case "stopped": return t("content.stopped");
      default: return t("content.error");
    }
  };

  function toggle() {
    if (open()) close();
    else {
      setOpen(true);
      queueMicrotask(() => panel?.querySelector<HTMLElement>("button:not(:disabled), input")?.focus());
    }
  }
  function close(refocus = true) {
    setOpen(false);
    if (refocus) button?.focus();
  }

  const onDocMouseDown = (e: MouseEvent) => {
    if (open() && !wrapper.contains(e.target as Node)) close(false);
  };
  document.addEventListener("mousedown", onDocMouseDown);
  onCleanup(() => document.removeEventListener("mousedown", onDocMouseDown));

  const pct = (s: ContentStatus) => (s.total > 0 ? Math.min(100, (s.processed / s.total) * 100) : 0);

  return (
    <Show when={status()}>
      {(s) => (
        <div class="content-status" ref={wrapper}>
          <button
            ref={button}
            type="button"
            class={`content-btn content-${s().state}`}
            aria-expanded={open()}
            aria-controls={PANEL_ID}
            aria-haspopup="dialog"
            title={t("content.title")}
            onClick={toggle}
          >
            <span class="content-label">{summary(s())}</span>
            <Show when={showsProgress(s())}>
              <span class="content-bar" aria-hidden="true">
                <span class="content-bar-fill" style={{ "inline-size": `${pct(s())}%` }} />
              </span>
            </Show>
          </button>

          <Show when={open()}>
            <div
              ref={panel}
              id={PANEL_ID}
              class="content-panel"
              role="dialog"
              aria-labelledby="content-title"
              onKeyDown={(e) => {
                if (e.key === "Escape") {
                  e.preventDefault();
                  e.stopPropagation();
                  close();
                }
              }}
            >
              <h2 id="content-title" class="drives-title">{t("content.title")}</h2>
              <p class="content-line" classList={{ "content-error-text": s().state === "error" }} role="status">
                <Switch fallback={summary(s())}>
                  <Match when={s().state === "running"}>
                    {t("content.runningDetail", { rate: formatCount(Math.round(s().docsPerSecond), locale()) })}
                  </Match>
                  <Match when={s().state === "error"}>
                    {t("content.errorDetail", { message: `⁨${s().error ?? ""}⁩` })}
                  </Match>
                  <Match when={s().state === "disabled"}>{t("content.disabledDetail")}</Match>
                </Switch>
              </p>
              {/* Paused: what happens next (it resumes by itself unless the user paused it). */}
              <Show when={s().state === "paused"}>
                <p class="content-line content-hint">{t(`content.pausedDetail.${pauseCause(s())}` as MessageKey)}</p>
              </Show>
              <Show when={showsProgress(s())}>
                <div
                  class="content-meter"
                  classList={{ paused: s().state === "paused" }}
                  role="progressbar"
                  aria-label={t("content.progressLabel")}
                  aria-valuemin={0}
                  aria-valuemax={s().total}
                  aria-valuenow={s().processed}
                  aria-valuetext={t("content.progress", {
                    processed: formatCount(s().processed, locale()), total: formatCount(s().total, locale()),
                  })}
                >
                  <span class="content-bar-fill" style={{ "inline-size": `${pct(s())}%` }} />
                </div>
                <p class="content-numbers">
                  <bdi>{formatCount(s().processed, locale())}</bdi> / <bdi>{formatCount(s().total, locale())}</bdi>
                </p>
              </Show>
              <dl class="content-facts">
                <dt>{t("content.indexed")}</dt>
                <dd>{formatCount(s().indexed, locale())}</dd>
                <Show when={s().failed > 0}>
                  <dt>{t("content.failed")}</dt>
                  <dd>{formatCount(s().failed, locale())}</dd>
                </Show>
              </dl>

              <div class="content-actions">
                {/* One button whose action flips, so keyboard focus stays on it (not disabled while
                    busy for the same reason: a second click is ignored instead). */}
                <button
                  type="button"
                  class={userPaused() ? "btn-primary" : "btn-secondary"}
                  disabled={!userPaused() && !canPause()}
                  onClick={() => {
                    if (!busy()) void run(userPaused() ? api.contentResume : api.contentPause);
                  }}
                >
                  {userPaused() ? t("content.resume") : t("content.pause")}
                </button>
              </div>

              <Show when={props.onOpenSettings}>
                <div class="content-switches">
                  <button type="button" class="btn-secondary" onClick={() => {
                    close(false);
                    props.onOpenSettings?.();
                  }}>
                    {t("content.moreSettings")}
                  </button>
                </div>
              </Show>
            </div>
          </Show>
        </div>
      )}
    </Show>
  );
}
