import { createSignal, For, onCleanup, Show } from "solid-js";
import { t, type MessageKey } from "../i18n";
import { api, errorMessage, type Volume } from "../lib/api";
import { rootKey, rootName } from "../lib/scope";

type Props = {
  /** Called with a message when saving the choice fails. */
  onError: (message: string) => void;
};

/** A row of the picker: a drive, or an indexed folder that is not a drive. */
type Row = { path: string; name: string; detail: string };

const DriveIcon = () => (
  <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75"
    stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
    <rect x="3" y="13" width="18" height="7" rx="2" />
    <path d="M5 13 7.5 5h9L19 13M7 16.5h.01" />
  </svg>
);

/**
 * "Which drives to index" (main window top bar): a button opening a small panel with a
 * "whole computer" switch and one checkbox per drive. Saving calls `set_index_roots`, which
 * persists the choice on the Rust side and re-indexes. Esc or a click outside closes the panel
 * without saving.
 */
export default function IndexSettings(props: Props) {
  const [open, setOpen] = createSignal(false);
  const [loading, setLoading] = createSignal(false);
  const [saving, setSaving] = createSignal(false);
  const [rows, setRows] = createSignal<Row[]>([]);
  const [whole, setWhole] = createSignal(true);
  /** Root keys (see rootKey) of the checked rows. */
  const [checked, setChecked] = createSignal<Set<string>>(new Set());
  /** What "whole computer" indexes now (shown checked and disabled while it is on). */
  const [wholeRoots, setWholeRoots] = createSignal<Set<string>>(new Set());
  let wrapper!: HTMLDivElement;
  let button!: HTMLButtonElement;
  let panel: HTMLDivElement | undefined;

  const kindLabel = (v: Volume) => t(`drives.kind.${v.kind}` as MessageKey);

  async function load() {
    setLoading(true);
    try {
      const [volumes, current] = await Promise.all([api.listVolumes(), api.getIndexRoots()]);
      const list: Row[] = volumes.map((v) => ({
        path: v.path,
        name: v.name,
        detail: v.label ? `${v.label} · ${kindLabel(v)}` : kindLabel(v),
      }));
      // Indexed folders that are not drives (or drives no longer present) stay visible.
      for (const r of current.roots) {
        if (!list.some((x) => rootKey(x.path) === rootKey(r))) list.push({ path: r, name: rootName(r), detail: t("drives.folder") });
      }
      const keys = new Set(current.roots.map(rootKey));
      setRows(list);
      setWhole(current.wholeComputer);
      setChecked(keys);
      setWholeRoots(
        current.wholeComputer
          ? keys
          : new Set(volumes.filter((v) => v.kind === "fixed").map((v) => rootKey(v.path))),
      );
    } catch (err) {
      props.onError(errorMessage(err));
      setOpen(false);
    } finally {
      setLoading(false);
    }
  }

  function toggle() {
    if (open()) close();
    else {
      setOpen(true);
      void load().then(() => panel?.querySelector<HTMLInputElement>("input:not(:disabled)")?.focus());
    }
  }

  function close(refocus = true) {
    setOpen(false);
    if (refocus) button.focus();
  }

  function flip(key: string, on: boolean) {
    const next = new Set(checked());
    if (on) next.add(key);
    else next.delete(key);
    setChecked(next);
  }

  const isChecked = (row: Row) => (whole() ? wholeRoots() : checked()).has(rootKey(row.path));
  const nothingChosen = () => !whole() && checked().size === 0;

  async function save() {
    if (nothingChosen()) return;
    setSaving(true);
    try {
      const roots = whole() ? null : rows().filter((r) => checked().has(rootKey(r.path))).map((r) => r.path);
      await api.setIndexRoots(roots);
      close();
    } catch (err) {
      props.onError(errorMessage(err));
    } finally {
      setSaving(false);
    }
  }

  // A click outside the panel closes it.
  const onDocMouseDown = (e: MouseEvent) => {
    if (open() && !wrapper.contains(e.target as Node)) close(false);
  };
  document.addEventListener("mousedown", onDocMouseDown);
  onCleanup(() => document.removeEventListener("mousedown", onDocMouseDown));

  return (
    <div class="drives" ref={wrapper}>
      <button
        ref={button}
        type="button"
        class="drives-btn"
        aria-expanded={open()}
        aria-controls="drives-panel"
        aria-haspopup="dialog"
        title={t("drives.title")}
        onClick={toggle}
      >
        <DriveIcon />
        <span>{t("drives.button")}</span>
      </button>
      <Show when={open()}>
        <div
          ref={panel}
          id="drives-panel"
          class="drives-panel"
          role="dialog"
          aria-labelledby="drives-title"
          onKeyDown={(e) => {
            if (e.key === "Escape") {
              e.preventDefault();
              e.stopPropagation();
              close();
            }
          }}
        >
          <h2 id="drives-title" class="drives-title">{t("drives.title")}</h2>
          <Show when={!loading()} fallback={<p class="drives-note">{t("drives.loading")}</p>}>
            <label class="drives-row drives-whole">
              <input type="checkbox" checked={whole()} onChange={(e) => setWhole(e.currentTarget.checked)} />
              <span>{t("drives.whole")}</span>
            </label>
            <ul class="drives-list" aria-label={t("drives.listLabel")}>
              <For each={rows()}>
                {(row) => (
                  <li>
                    <label class="drives-row" classList={{ disabled: whole() }}>
                      <input
                        type="checkbox"
                        disabled={whole()}
                        checked={isChecked(row)}
                        onChange={(e) => flip(rootKey(row.path), e.currentTarget.checked)}
                      />
                      <bdi class="drives-name" dir="ltr">{row.name}</bdi>
                      <span class="drives-detail">{row.detail}</span>
                    </label>
                  </li>
                )}
              </For>
            </ul>
            <p class="drives-note" role={nothingChosen() ? "alert" : undefined}>
              {nothingChosen() ? t("drives.none") : t("drives.hint")}
            </p>
            <div class="drives-actions">
              <button type="button" class="btn-secondary" onClick={() => close()}>{t("drives.cancel")}</button>
              <button type="button" class="btn-primary" disabled={nothingChosen() || saving()} onClick={() => void save()}>
                {t("drives.apply")}
              </button>
            </div>
          </Show>
        </div>
      </Show>
    </div>
  );
}
