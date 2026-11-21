import { createSignal, For, Match, onMount, Show, Switch } from "solid-js";
import { t, type MessageKey } from "../i18n";
import { api, errorMessage } from "../lib/api";
import { setSearchContent } from "../lib/searchContent";
import { storedFlag } from "../lib/storage";
import DrivePicker, { type DrivePickerHandle } from "./IndexSettings";
import Modal from "./Modal";

/** Set once the welcome screen was finished or skipped; "show it again" in settings clears it. */
export const [welcomeDone, setWelcomeDone] = storedFlag("welcomeDone", false);

type Level = "names" | "content" | "smart";
const LEVELS: { value: Level; title: MessageKey; body: MessageKey; soon?: boolean }[] = [
  { value: "names", title: "welcome.levelNames", body: "welcome.levelNamesBody" },
  { value: "content", title: "welcome.levelContent", body: "welcome.levelContentBody" },
  { value: "smart", title: "welcome.levelSmart", body: "welcome.levelSmartBody", soon: true },
];

const STEPS = 3;

/** Burgundy line drawings, one per step (design §9.3). Decorative. */
function Art(props: { step: number }) {
  const common = {
    width: 168, height: 104, viewBox: "0 0 168 104", fill: "none", stroke: "currentColor",
    "stroke-width": 1.75, "stroke-linecap": "round", "stroke-linejoin": "round", "aria-hidden": true,
  } as const;
  return (
    <Switch>
      <Match when={props.step === 0}>
        {/* Two drives and a folder under a lens. */}
        <svg {...common} class="welcome-art">
          <rect x="18" y="58" width="56" height="20" rx="5" />
          <path d="M26 68h.01M34 68h20" />
          <rect x="18" y="30" width="56" height="20" rx="5" opacity=".55" />
          <path d="M26 40h.01M34 40h20" opacity=".55" />
          <path d="M96 34a5 5 0 0 1 5-5h12l5 6h22a5 5 0 0 1 5 5v28a5 5 0 0 1-5 5h-39a5 5 0 0 1-5-5Z" />
          <circle cx="128" cy="58" r="12" />
          <path d="m137 67 9 9" />
          <path d="M80 54c4-6 8-8 12-8" stroke-dasharray="2 5" />
        </svg>
      </Match>
      <Match when={props.step === 1}>
        {/* A name tag, then a page with lines: names, then contents. */}
        <svg {...common} class="welcome-art">
          <path d="M20 40h40l8 10-8 10H20a4 4 0 0 1-4-4V44a4 4 0 0 1 4-4Z" />
          <path d="M26 50h22" />
          <path d="M78 50h12" stroke-dasharray="2 5" />
          <path d="M104 18h28l14 14v52a4 4 0 0 1-4 4h-38a4 4 0 0 1-4-4V22a4 4 0 0 1 4-4Z" />
          <path d="M132 18v14h14M110 44h28M110 54h28M110 64h18" />
        </svg>
      </Match>
      <Match when={props.step === 2}>
        {/* Ctrl + Shift + Space keys. */}
        <svg {...common} class="welcome-art">
          <rect x="10" y="38" width="34" height="28" rx="6" />
          <path d="M20 52h14" />
          <path d="M50 52h6M53 49v6" />
          <rect x="62" y="38" width="34" height="28" rx="6" />
          <path d="M79 58v-12m-5 5 5-5 5 5" />
          <path d="M102 52h6M105 49v6" />
          <rect x="114" y="38" width="46" height="28" rx="6" />
          <path d="M124 56v3h26v-3" />
        </svg>
      </Match>
    </Switch>
  );
}

type Props = {
  onDone: () => void;
  onNotice: (message: string) => void;
};

/**
 * The welcome screen (task 5.1): three steps, where to search, how deep to index, and the global
 * shortcut. Shown on first run in the main window; skippable at any step (nothing is changed
 * then). "Next" applies each step's choice.
 */
export default function Onboarding(props: Props) {
  const [step, setStep] = createSignal(0);
  const [busy, setBusy] = createSignal(false);
  const [level, setLevel] = createSignal<Level>("names");
  const [error, setError] = createSignal<string | null>(null);
  let drives: DrivePickerHandle | undefined;

  onMount(async () => {
    try {
      const s = await api.contentStatus();
      setLevel(s.state === "disabled" ? "names" : "content");
    } catch {
      // An older backend without content indexing: names only.
    }
  });

  function finish() {
    setWelcomeDone(true);
    props.onDone();
  }

  async function next() {
    if (busy()) return;
    setError(null);
    setBusy(true);
    try {
      if (step() === 0) {
        if (!(await drives?.apply())) return;
      } else if (step() === 1) {
        const withContent = level() === "content";
        await api.setContentIndexing(withContent);
        setSearchContent(withContent);
      } else {
        finish();
        return;
      }
      setStep(step() + 1);
      document.getElementById("welcome-step-title")?.focus();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  }

  function back() {
    setError(null);
    setStep(Math.max(0, step() - 1));
    document.getElementById("welcome-step-title")?.focus();
  }

  const titleKey = (): MessageKey => (["welcome.whereTitle", "welcome.levelTitle", "welcome.shortcutTitle"] as const)[step()];
  const bodyKey = (): MessageKey => (["welcome.whereBody", "welcome.levelBody", "welcome.shortcutBody"] as const)[step()];

  return (
    <Modal labelledBy="welcome-title" describedBy="welcome-step-body" onClose={finish} class="welcome" closeOnBackdrop={false}>
      <header class="welcome-head">
        <img src="/athrix.svg" alt="" width="28" height="28" />
        <h2 id="welcome-title" class="welcome-brand">{t("welcome.title")}</h2>
        <button type="button" class="btn-ghost" onClick={finish}>{t("welcome.skip")}</button>
      </header>

      <div class="welcome-art-wrap"><Art step={step()} /></div>

      <p class="welcome-progress" aria-live="polite">
        <span class="welcome-dots" aria-hidden="true">
          <For each={Array.from({ length: STEPS }, (_, i) => i)}>
            {(i) => <span class="welcome-dot" classList={{ on: i === step(), done: i < step() }} />}
          </For>
        </span>
        <span>{t("welcome.step", { n: step() + 1, total: STEPS })}</span>
      </p>
      <h3 id="welcome-step-title" class="welcome-step-title" tabIndex={-1}>{t(titleKey())}</h3>
      <p id="welcome-step-body" class="welcome-body">{t(bodyKey())}</p>

      <div class="welcome-content">
        <Switch>
          <Match when={step() === 0}>
            <DrivePicker
              mode="embedded"
              labelledBy="welcome-step-title"
              ref={(h) => (drives = h)}
              onError={(message) => setError(message)}
            />
          </Match>
          <Match when={step() === 1}>
            <div class="level-list" role="radiogroup" aria-labelledby="welcome-step-title">
              <For each={LEVELS}>
                {(l) => (
                  <label class="level-card" classList={{ selected: level() === l.value, disabled: l.soon }}>
                    <input
                      type="radio"
                      name="welcome-level"
                      value={l.value}
                      checked={level() === l.value}
                      disabled={l.soon}
                      onChange={() => setLevel(l.value)}
                    />
                    <span class="level-text">
                      <span class="level-title">
                        {t(l.title)}
                        <Show when={l.soon}><span class="soon-badge">{t("welcome.soon")}</span></Show>
                      </span>
                      <span class="level-body">{t(l.body)}</span>
                    </span>
                  </label>
                )}
              </For>
            </div>
            <p class="drives-note">{t("welcome.levelLater")}</p>
          </Match>
          <Match when={step() === 2}>
            <p class="welcome-keys" dir="ltr">
              <kbd>Ctrl</kbd><span aria-hidden="true">+</span><kbd>Shift</kbd><span aria-hidden="true">+</span><kbd>Space</kbd>
            </p>
            <ul class="welcome-tips">
              <li>{t("welcome.tipAnywhere")}</li>
              <li>{t("welcome.tipEnter")}</li>
              <li>{t("welcome.tipSettings")}</li>
            </ul>
          </Match>
        </Switch>
      </div>

      <Show when={error()}>
        <p class="field-error" role="alert">{t("welcome.failed", { message: error()! })}</p>
      </Show>

      <footer class="welcome-actions">
        <Show when={step() > 0}>
          <button type="button" class="btn-secondary" onClick={back} disabled={busy()}>{t("welcome.back")}</button>
        </Show>
        <button type="button" class="btn-primary" onClick={() => void next()} disabled={busy()}>
          {step() === STEPS - 1 ? t("welcome.finish") : t("welcome.next")}
        </button>
      </footer>
    </Modal>
  );
}
