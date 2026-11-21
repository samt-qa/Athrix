import { createSignal, For, onMount, Show, type JSX } from "solid-js";
import { locale, t } from "../i18n";
import { api, errorMessage, type Favorite, type SavedSearch } from "../lib/api";
import { categoryOf } from "../lib/fileKinds";
import { formatRelative } from "../lib/format";
import {
  deleteSaved, favorites, history, historyEnabled, refreshLibrary, removeFavorite, removeHistory, renameSaved, saved,
} from "../lib/library";
import { rootName } from "../lib/scope";
import { TypeIcon } from "./icons";
import PromptDialog from "./PromptDialog";

type Props = {
  /** Run a past or saved search: fill the field (and the scope it ran in). */
  onRun: (query: string, scope: string | null) => void;
  onNotice: (message: string) => void;
  /** Focus goes back to the search field (ArrowUp past the first item, Esc). */
  onExit: () => void;
  /** The quick-search popup: recent searches only, fewer of them. */
  compact?: boolean;
  /** Rendered instead when there is nothing to show. */
  fallback: JSX.Element;
  /** Rendered above the lists when they show (e.g. the "still indexing" note). */
  before?: JSX.Element;
  ref?: (handle: LibraryHandle) => void;
  now: number;
};

export type LibraryHandle = {
  /** Moves focus to the first item; false when there is none. */
  focusFirst: () => boolean;
};

const ITEM = "[data-lib-item]";

const s = { fill: "none", stroke: "currentColor", "stroke-width": 1.75, "stroke-linecap": "round", "stroke-linejoin": "round", "aria-hidden": true } as const;
const ClockIcon = () => <svg width="14" height="14" viewBox="0 0 24 24" {...s}><circle cx="12" cy="12" r="8.5" /><path d="M12 7.5V12l3 2" /></svg>;
const BookmarkIcon = () => <svg width="14" height="14" viewBox="0 0 24 24" {...s}><path d="M7 4h10a1 1 0 0 1 1 1v15l-6-4-6 4V5a1 1 0 0 1 1-1Z" /></svg>;
const XIcon = () => <svg width="14" height="14" viewBox="0 0 24 24" {...s}><path d="M7 7l10 10M17 7 7 17" /></svg>;
const PencilIcon = () => <svg width="14" height="14" viewBox="0 0 24 24" {...s}><path d="M5 19h3.5L19 8.5 15.5 5 5 15.5Z" /></svg>;
const FolderOpenIcon = () => <svg width="14" height="14" viewBox="0 0 24 24" {...s}><path d="M3.5 7.5a2 2 0 0 1 2-2h3.6l2 2.2h7.4a2 2 0 0 1 2 2v1.3M3.5 7.5v10a2 2 0 0 0 2 2h12l3-8H7.5l-4 6" /></svg>;
export const StarIcon = (p: { filled?: boolean }) => (
  <svg width="14" height="14" viewBox="0 0 24 24" {...s} fill={p.filled ? "currentColor" : "none"}>
    <path d="m12 4 2.4 5 5.4.7-4 3.7 1 5.4L12 16.2 7.2 18.8l1-5.4-4-3.7 5.4-.7Z" />
  </svg>
);

const extOf = (name: string) => {
  const i = name.lastIndexOf(".");
  return i > 0 ? name.slice(i + 1).toLowerCase() : "";
};

/**
 * What the empty search box shows (task 5.3): favorites, saved searches and recent searches.
 * Each item is a button (Enter / click runs or opens it) with small secondary actions. Arrow
 * keys move between items; ArrowUp past the first one or Esc returns to the search field.
 */
export default function LibraryPanel(props: Props) {
  const [renaming, setRenaming] = createSignal<SavedSearch | null>(null);
  let root: HTMLDivElement | undefined;

  onMount(() => void refreshLibrary());
  props.ref?.({
    focusFirst: () => {
      const first = root?.querySelector<HTMLElement>(ITEM);
      first?.focus();
      return !!first;
    },
  });

  const recent = () => (historyEnabled() ? history().slice(0, props.compact ? 5 : 8) : []);
  const savedList = () => (props.compact ? [] : saved());
  const favList = () => (props.compact ? [] : favorites());
  const empty = () => !recent().length && !savedList().length && !favList().length;

  function onKeyDown(e: KeyboardEvent) {
    if (!root) return;
    const items = [...root.querySelectorAll<HTMLElement>(ITEM)];
    const row = (e.target as HTMLElement).closest(".lib-item");
    const i = items.findIndex((el) => el.closest(".lib-item") === row);
    switch (e.key) {
      case "ArrowDown":
        if (i < items.length - 1) items[i + 1].focus();
        break;
      case "ArrowUp":
        if (i > 0) items[i - 1].focus();
        else props.onExit();
        break;
      case "Escape":
        props.onExit();
        break;
      default:
        return;
    }
    e.preventDefault();
    e.stopPropagation();
  }

  async function act(run: () => Promise<unknown>, done?: string) {
    try {
      await run();
      if (done) props.onNotice(done);
    } catch (err) {
      props.onNotice(t("library.actionFailed", { message: errorMessage(err) }));
    }
  }

  /** Runs a removal, then keeps the keyboard focus on the item that took its place. */
  async function remove(e: MouseEvent, run: () => Promise<unknown>, done?: string) {
    const items = [...(root?.querySelectorAll<HTMLElement>(ITEM) ?? [])];
    const row = (e.currentTarget as HTMLElement).closest(".lib-item");
    const i = items.findIndex((el) => el.closest(".lib-item") === row);
    await act(run, done);
    queueMicrotask(() => {
      const after = [...(root?.querySelectorAll<HTMLElement>(ITEM) ?? [])];
      const target = after[Math.min(Math.max(i, 0), after.length - 1)];
      if (target && root?.isConnected) target.focus();
      else props.onExit();
    });
  }

  const openFavorite = (f: Favorite, reveal: boolean) =>
    act(
      () => (reveal ? api.favoriteReveal(f.path) : api.favoriteOpen(f.path)),
      t(reveal ? "notice.revealing" : "notice.opening", { name: f.name }),
    );

  const scopeBadge = (scope: string | null) => (
    <Show when={scope}>
      {(sc) => <bdi class="lib-scope" dir="ltr" title={t("library.inScope", { scope: sc() })}>{rootName(sc())}</bdi>}
    </Show>
  );

  return (
    <Show when={!empty()} fallback={props.fallback}>
      {props.before}
      <div ref={root} class="library" classList={{ compact: props.compact }} onKeyDown={onKeyDown}>
        <Show when={favList().length}>
          <section class="lib-section" aria-labelledby="lib-fav">
            <h2 id="lib-fav" class="lib-title"><StarIcon />{t("library.favorites")}</h2>
            <ul class="lib-list">
              <For each={favList()}>
                {(f) => (
                  <li class="lib-item" classList={{ missing: !f.exists }}>
                    <button type="button" class="lib-main" data-lib-item disabled={!f.exists}
                      title={f.exists ? f.path : t("library.missing")}
                      onClick={() => void openFavorite(f, false)}>
                      <TypeIcon category={categoryOf(extOf(f.name), f.isDir)} />
                      <span class="lib-text">
                        <bdi class="lib-name" dir="auto">{f.name}</bdi>
                        <bdi class="lib-path" dir="ltr">{f.path}</bdi>
                      </span>
                      <Show when={!f.exists}><span class="lib-missing">{t("library.missing")}</span></Show>
                    </button>
                    <span class="lib-actions">
                      <Show when={f.exists}>
                        <button type="button" class="icon-btn" title={t("library.reveal")} aria-label={`${t("library.reveal")}: ${f.name}`}
                          onClick={() => void openFavorite(f, true)}><FolderOpenIcon /></button>
                      </Show>
                      <button type="button" class="icon-btn" title={t("library.unfavorite")} aria-label={`${t("library.unfavorite")}: ${f.name}`}
                        onClick={(e) => void remove(e, () => removeFavorite(f.path), t("library.unfavorited", { name: f.name }))}><XIcon /></button>
                    </span>
                  </li>
                )}
              </For>
            </ul>
          </section>
        </Show>

        <Show when={savedList().length}>
          <section class="lib-section" aria-labelledby="lib-saved">
            <h2 id="lib-saved" class="lib-title"><BookmarkIcon />{t("library.saved")}</h2>
            <ul class="lib-list">
              <For each={savedList()}>
                {(sv) => (
                  <li class="lib-item">
                    <button type="button" class="lib-main" data-lib-item onClick={() => props.onRun(sv.query, sv.scope)}>
                      <span class="lib-text">
                        <bdi class="lib-name" dir="auto">{sv.name}</bdi>
                        <bdi class="lib-query" dir="auto">{sv.query}</bdi>
                      </span>
                      {scopeBadge(sv.scope)}
                    </button>
                    <span class="lib-actions">
                      <button type="button" class="icon-btn" title={t("library.rename")} aria-label={`${t("library.rename")}: ${sv.name}`}
                        onClick={() => setRenaming(sv)}><PencilIcon /></button>
                      <button type="button" class="icon-btn" title={t("library.deleteSaved")} aria-label={`${t("library.deleteSaved")}: ${sv.name}`}
                        onClick={(e) => void remove(e, () => deleteSaved(sv.id), t("library.deleted", { name: sv.name }))}><XIcon /></button>
                    </span>
                  </li>
                )}
              </For>
            </ul>
          </section>
        </Show>

        <Show when={recent().length}>
          <section class="lib-section" aria-labelledby="lib-recent">
            <h2 id="lib-recent" class="lib-title"><ClockIcon />{t("library.recent")}</h2>
            <ul class="lib-list">
              <For each={recent()}>
                {(h) => (
                  <li class="lib-item">
                    <button type="button" class="lib-main" data-lib-item onClick={() => props.onRun(h.query, h.scope)}>
                      <span class="lib-text lib-text-row">
                        <bdi class="lib-query lib-query-main" dir="auto">{h.query}</bdi>
                      </span>
                      {scopeBadge(h.scope)}
                      <span class="lib-time">{formatRelative(h.searchedAt / 1000, locale(), props.now)}</span>
                    </button>
                    <span class="lib-actions">
                      <button type="button" class="icon-btn" title={t("library.removeRecent")} aria-label={`${t("library.removeRecent")}: ${h.query}`}
                        onClick={(e) => void remove(e, () => removeHistory(h.id))}><XIcon /></button>
                    </span>
                  </li>
                )}
              </For>
            </ul>
          </section>
        </Show>
      </div>

      <Show when={renaming()}>
        {(sv) => (
          <PromptDialog
            title={t("library.renameTitle")}
            label={t("library.nameLabel")}
            detail={sv().query}
            initial={sv().name}
            confirm={t("library.renameConfirm")}
            onSubmit={(name) => renameSaved(sv().id, name)}
            onClose={() => setRenaming(null)}
          />
        )}
      </Show>
    </Show>
  );
}
