import { createMemo, createSignal, For, onMount, Show } from "solid-js";
import { t, type MessageKey } from "../i18n";
import { api, errorMessage, type Volume } from "../lib/api";
import { rootKey, rootName } from "../lib/scope";

export type DrivePickerHandle = {
  /** Saves the choice if it changed (`set_index_roots`, which re-indexes). False when nothing is chosen or saving failed. */
  apply: () => Promise<boolean>;
};

type Props = {
  /** Called with a message when loading or saving fails. */
  onError: (message: string) => void;
  /**
   * "settings": a "save and re-index" button under the list, enabled once the choice changed.
   * "embedded": no button; the owner calls `apply()` (the welcome screen's "next").
   */
  mode: "settings" | "embedded";
  ref?: (handle: DrivePickerHandle) => void;
  /** Id for the list's heading, if the owner shows one. */
  labelledBy?: string;
};

/** A row of the picker: a drive, or an indexed folder that is not a drive. */
type Row = { path: string; name: string; detail: string };

const sameSet = (a: Set<string>, b: Set<string>) => a.size === b.size && [...a].every((x) => b.has(x));

/**
 * "Where to search" (settings → indexing, and the welcome screen): a "whole computer" switch and
 * one checkbox per drive. Saving calls `set_index_roots`, which persists the choice on the Rust
 * side and re-indexes.
 */
export default function DrivePicker(props: Props) {
  const [loading, setLoading] = createSignal(true);
  const [saving, setSaving] = createSignal(false);
  const [rows, setRows] = createSignal<Row[]>([]);
  const [whole, setWhole] = createSignal(true);
  /** Root keys (see rootKey) of the checked rows. */
  const [checked, setChecked] = createSignal<Set<string>>(new Set());
  /** What "whole computer" indexes now (shown checked and disabled while it is on). */
  const [wholeRoots, setWholeRoots] = createSignal<Set<string>>(new Set());
  /** The saved choice, to tell whether anything changed. */
  const [savedChoice, setSavedChoice] = createSignal<{ whole: boolean; keys: Set<string> }>({ whole: true, keys: new Set() });
  const [note, setNote] = createSignal<string | null>(null);

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
      setSavedChoice({ whole: current.wholeComputer, keys });
      setWholeRoots(
        current.wholeComputer
          ? keys
          : new Set(volumes.filter((v) => v.kind === "fixed").map((v) => rootKey(v.path))),
      );
    } catch (err) {
      props.onError(errorMessage(err));
    } finally {
      setLoading(false);
    }
  }
  onMount(load);

  function flip(key: string, on: boolean) {
    const next = new Set(checked());
    if (on) next.add(key);
    else next.delete(key);
    setChecked(next);
    setNote(null);
  }

  const isChecked = (row: Row) => (whole() ? wholeRoots() : checked()).has(rootKey(row.path));
  const nothingChosen = () => !loading() && !whole() && checked().size === 0;
  const changed = createMemo(() => {
    const s = savedChoice();
    if (whole() !== s.whole) return true;
    return !whole() && !sameSet(checked(), s.keys);
  });

  async function apply(): Promise<boolean> {
    if (loading() || nothingChosen()) return false;
    if (!changed()) return true;
    setSaving(true);
    try {
      const roots = whole() ? null : rows().filter((r) => checked().has(rootKey(r.path))).map((r) => r.path);
      const result = await api.setIndexRoots(roots);
      const keys = new Set(result.roots.map(rootKey));
      setSavedChoice({ whole: result.wholeComputer, keys });
      if (result.wholeComputer) setWholeRoots(keys);
      setNote(t("drives.saved"));
      return true;
    } catch (err) {
      props.onError(errorMessage(err));
      return false;
    } finally {
      setSaving(false);
    }
  }
  props.ref?.({ apply });

  return (
    <div class="drive-picker">
      <Show when={!loading()} fallback={<p class="drives-note">{t("drives.loading")}</p>}>
        <label class="drives-row drives-whole">
          <input
            type="checkbox"
            checked={whole()}
            onChange={(e) => {
              setWhole(e.currentTarget.checked);
              setNote(null);
            }}
          />
          <span>{t("drives.whole")}</span>
        </label>
        <ul class="drives-list" aria-label={props.labelledBy ? undefined : t("drives.listLabel")} aria-labelledby={props.labelledBy}>
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
        <p class="drives-note" role={nothingChosen() ? "alert" : "status"}>
          {nothingChosen() ? t("drives.none") : note() ?? (props.mode === "settings" ? t("drives.hint") : "")}
        </p>
        <Show when={props.mode === "settings"}>
          <div class="drives-actions">
            <button
              type="button"
              class="btn-primary"
              disabled={!changed() || nothingChosen() || saving()}
              onClick={() => void apply()}
            >
              {t("drives.apply")}
            </button>
          </div>
        </Show>
      </Show>
    </div>
  );
}
