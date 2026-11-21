import { batch, createEffect, createMemo, createSignal, For, Match, on, onCleanup, Show, Switch, type JSX } from "solid-js";
import { locale, t, tPlural, type MessageKey } from "../i18n";
import { api, errorMessage, isStaleIdError, type Preview, type PreviewBody, type Range16, type ResultItem } from "../lib/api";
import { categoryOf } from "../lib/fileKinds";
import { formatCount, formatDateTime, formatSize } from "../lib/format";
import { highlightSyntax, type SyntaxSpan } from "../lib/syntax";
import { findMatches, queryTerms } from "../lib/textMatch";
import { TypeIcon } from "./icons";

/** Text characters asked of the backend (its default too). */
const MAX_CHARS = 65_536;
/** Selection changes closer together than this (holding ↓) only preview the last one. */
const DEBOUNCE_MS = 70;

export type PreviewHandle = {
  nextMatch: () => void;
  prevMatch: () => void;
  /** Moves focus into the pane's body (for scrolling with the keyboard). */
  focusBody: () => void;
};

type Props = {
  id: string;
  /** The selected result; undefined = nothing to preview. */
  item: ResultItem | undefined;
  /** The search field's text: its words are highlighted in the preview. */
  query: string;
  onOpen: () => void;
  onReveal: () => void;
  onClose: () => void;
  /** The id is from an older index: the results must be refreshed. */
  onStale: () => void;
  ref?: (handle: PreviewHandle) => void;
};

/** One run of a rendered line: plain, colored (`cls`) and/or a match (`m` = match index). */
type Piece = { text: string; cls?: string; m?: number };

/**
 * The preview pane of the main window (design §9.2, §10 PreviewPane): a header with the entry's
 * icon, name, path and actions; a body by kind (text and code with line numbers, document
 * paragraphs, image, folder listing, or why there is no preview); metadata in a two-column
 * footer. The query's words are highlighted in the text (task 4.9) with previous/next buttons;
 * F3 / Shift+F3 come through the handle.
 */
export default function PreviewPane(props: Props) {
  let body: HTMLDivElement | undefined;
  const [preview, setPreview] = createSignal<Preview | null>(null);
  const [shownId, setShownId] = createSignal<number | null>(null);
  const [loading, setLoading] = createSignal(false);
  const [failure, setFailure] = createSignal<string | null>(null);
  const [dims, setDims] = createSignal<{ w: number; h: number } | null>(null);

  // ---- fetching: latest request wins (requestId like search), debounced ----
  let lastRequest = Date.now();
  let timer: ReturnType<typeof setTimeout> | undefined;
  onCleanup(() => clearTimeout(timer));
  createEffect(on(() => props.item?.id, (id) => {
    clearTimeout(timer);
    if (id === undefined) {
      ++lastRequest;
      batch(() => {
        setPreview(null);
        setShownId(null);
        setLoading(false);
        setFailure(null);
      });
      return;
    }
    setLoading(true);
    timer = setTimeout(() => void load(id), shownId() === null ? 0 : DEBOUNCE_MS);
  }));

  async function load(id: number) {
    const requestId = ++lastRequest;
    try {
      const r = await api.previewEntry(id, requestId, MAX_CHARS);
      if (requestId !== lastRequest || r.cancelled || !r.preview) return;
      batch(() => {
        setPreview(r.preview);
        setShownId(id);
        setDims(null);
        setFailure(null);
        setLoading(false);
      });
      body?.scrollTo({ top: 0 });
    } catch (err) {
      if (requestId !== lastRequest) return;
      batch(() => {
        setLoading(false);
        setPreview(null);
        setShownId(id);
        setFailure(errorMessage(err));
      });
      if (isStaleIdError(err)) props.onStale();
    }
  }

  // ---- text, matches and the current match ----
  const textBody = createMemo(() => {
    const b = preview()?.body;
    return b && (b.kind === "text" || b.kind === "code" || b.kind === "document") ? b : null;
  });
  const terms = createMemo(() => queryTerms(props.query), undefined, {
    equals: (a, b) => a.length === b.length && a.every((x, i) => x === b[i]),
  });
  const matches = createMemo<Range16[]>(() => {
    const b = textBody();
    return b ? findMatches(b.text, terms()) : [];
  });
  const [current, setCurrent] = createSignal(0);

  // Lines (text, code) or paragraphs (documents), split into colored / matched pieces.
  const lines = createMemo(() => {
    const b = textBody();
    if (!b) return [];
    const spans = b.kind === "code" ? highlightSyntax(b.text, b.language) : [];
    return splitLines(b.text, spans, matches());
  });

  // New text or new words: back to the first match, scrolled into view.
  createEffect(on(matches, (m) => {
    setCurrent(0);
    if (m.length) queueMicrotask(() => showMatch(0, true));
  }, { defer: true }));

  let marked: Element[] = [];
  function showMatch(i: number, gentle = false) {
    if (!body) return;
    for (const el of marked) el.classList.remove("current");
    marked = Array.from(body.querySelectorAll(`[data-m="${i}"]`));
    for (const el of marked) el.classList.add("current");
    marked[0]?.scrollIntoView({ block: gentle ? "nearest" : "center", inline: "nearest" });
  }
  function step(delta: number) {
    const n = matches().length;
    if (!n) return;
    const next = (current() + delta + n) % n;
    setCurrent(next);
    showMatch(next);
  }
  props.ref?.({
    nextMatch: () => step(1),
    prevMatch: () => step(-1),
    focusBody: () => body?.focus(),
  });

  // ---- header and footer ----
  const entry = () => preview()?.entry;
  const name = () => entry()?.name ?? props.item?.name ?? "";
  const path = () => entry()?.path ?? (props.item ? joinPath(props.item.parent, props.item.name) : "");
  const isDir = () => entry()?.isDir ?? props.item?.isDir ?? false;
  const ext = () => entry()?.ext ?? props.item?.ext ?? "";
  const stale = () => loading() && shownId() !== props.item?.id;

  const meta = createMemo(() => {
    const p = preview();
    if (!p) return [];
    const rows: [MessageKey, string, boolean?][] = [];
    const e = p.entry;
    const b = p.body;
    rows.push(["preview.meta.type", e.isDir ? t("preview.type.folder") : typeLabel(b, e.ext)]);
    if (e.size !== null) rows.push(["preview.meta.size", formatSize(e.size, locale())]);
    if (e.modified !== null) rows.push(["preview.meta.modified", formatDateTime(e.modified, locale())]);
    if (b.kind === "folder") rows.push(["preview.meta.contents", itemsLabel(b.childCount)]);
    if (b.kind === "document") {
      if (b.title) rows.push(["preview.meta.title", b.title, true]);
      if (b.author) rows.push(["preview.meta.author", b.author, true]);
    }
    if (b.kind === "text" || b.kind === "code") {
      rows.push(["preview.meta.lines", formatCount(lines().length, locale()) + (b.truncated ? "+" : "")]);
      if (b.encoding) rows.push(["preview.meta.encoding", b.encoding]);
    }
    const d = dims();
    if (b.kind === "image" && d) {
      rows.push(["preview.meta.dimensions", t("preview.dimensions", { width: formatCount(d.w, locale()), height: formatCount(d.h, locale()) })]);
    }
    return rows;
  });

  const onBodyKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      props.onClose();
    }
  };

  return (
    <aside id={props.id} class="preview" aria-label={t("preview.region")} aria-busy={loading()}>
      <header class="pv-head">
        <Show when={props.item}>
          <TypeIcon category={categoryOf(ext(), isDir())} />
        </Show>
        <div class="pv-title">
          <h2 class="pv-name" dir="auto" title={name()}>{name()}</h2>
          <Show when={path()}>
            <bdi class="pv-path" dir="ltr" title={path()}>{path()}</bdi>
          </Show>
        </div>
        <div class="pv-actions">
          <Show when={props.item}>
            <button type="button" class="btn-ghost" onClick={() => props.onOpen()}>{t("preview.open")}</button>
            <button type="button" class="btn-ghost" onClick={() => props.onReveal()}>{t("preview.reveal")}</button>
          </Show>
          <button type="button" class="icon-btn" aria-label={t("preview.hide")} title={t("preview.hide")}
            aria-controls={props.id} aria-expanded="true" onClick={() => props.onClose()}>
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75"
              stroke-linecap="round" aria-hidden="true"><path d="M6 6l12 12M18 6 6 18" /></svg>
          </button>
        </div>
      </header>

      <Show when={textBody() && terms().length}>
        <div class="pv-matchbar" role="group" aria-label={t("preview.matchesLabel")}>
          <span class="pv-matchcount" aria-live="polite">
            {matches().length
              ? t("preview.matchCount", { current: formatCount(current() + 1, locale()), total: formatCount(matches().length, locale()) })
              : t("preview.matchNone")}
          </span>
          <button type="button" class="icon-btn" disabled={!matches().length} aria-label={t("preview.prevMatch")}
            title={t("preview.prevMatch")} onMouseDown={(e) => e.preventDefault()} onClick={() => step(-1)}>
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75"
              stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="m6 15 6-6 6 6" /></svg>
          </button>
          <button type="button" class="icon-btn" disabled={!matches().length} aria-label={t("preview.nextMatch")}
            title={t("preview.nextMatch")} onMouseDown={(e) => e.preventDefault()} onClick={() => step(1)}>
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75"
              stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="m6 9 6 6 6-6" /></svg>
          </button>
        </div>
      </Show>

      <div
        ref={body}
        class="pv-body"
        classList={{ "pv-stale": stale() }}
        tabIndex={0}
        role="region"
        aria-label={t("preview.bodyLabel", { name: name() })}
        onKeyDown={onBodyKey}
      >
        <Switch>
          <Match when={!props.item}>
            <p class="pv-note">{t("preview.empty")}</p>
          </Match>
          <Match when={failure()}>
            {(msg) => <p class="pv-note" role="alert">{t("preview.failed", { message: msg() })}</p>}
          </Match>
          <Match when={!preview()}>
            <p class="pv-note">{t("preview.loading")}</p>
          </Match>
          <Match when={textBody()}>
            {(b) => (
              <>
                <Show
                  when={b().kind === "document"}
                  fallback={
                    <div class="pv-code" classList={{ "pv-prose": b().kind === "text" }}>
                      <For each={lines()}>
                        {(line, i) => (
                          <div class="pv-line">
                            <span class="pv-ln" aria-hidden="true">{i() + 1}</span>
                            <span class="pv-text" dir={b().kind === "code" ? "ltr" : "auto"}><Pieces pieces={line} /></span>
                          </div>
                        )}
                      </For>
                    </div>
                  }
                >
                  <div class="pv-doc">
                    <For each={lines()}>
                      {(para) => (para.length ? <p dir="auto"><Pieces pieces={para} /></p> : null)}
                    </For>
                  </div>
                </Show>
                <Show when={b().truncated}>
                  <p class="pv-truncated">
                    {t("preview.truncated")}
                    <button type="button" class="link-btn" onClick={() => props.onOpen()}>{t("preview.open")}</button>
                  </p>
                </Show>
              </>
            )}
          </Match>
          <Match when={bodyOf(preview(), "image")}>
            {(img) => (
              <div class="pv-image">
                <img
                  src={img().src}
                  alt={t("preview.imageAlt", { name: name() })}
                  decoding="async"
                  onLoad={(e) => setDims({ w: e.currentTarget.naturalWidth, h: e.currentTarget.naturalHeight })}
                />
              </div>
            )}
          </Match>
          <Match when={bodyOf(preview(), "folder")}>
            {(f) => (
              <>
                <p class="pv-subtle">{itemsLabel(f().childCount)}</p>
                <ul class="pv-folder" aria-label={name()}>
                  <For each={f().children}>
                    {(c) => (
                      <li>
                        <TypeIcon category={categoryOf(extOf(c.name), c.isDir)} />
                        <bdi dir="auto">{c.name}</bdi>
                      </li>
                    )}
                  </For>
                </ul>
                <Show when={f().childCount > f().children.length}>
                  <p class="pv-subtle">{t("preview.more", { count: formatCount(f().childCount - f().children.length, locale()) })}</p>
                </Show>
              </>
            )}
          </Match>
          <Match when={bodyOf(preview(), "unsupported")}>
            {(u) => (
              <div class="pv-unsupported">
                <TypeIcon category={categoryOf(ext(), false)} />
                <p>{t(`preview.reason.${u().reason}` as MessageKey)}</p>
                <button type="button" class="btn-primary" onClick={() => props.onOpen()}>{t("preview.open")}</button>
              </div>
            )}
          </Match>
        </Switch>
      </div>

      <Show when={meta().length}>
        <dl class="pv-meta" aria-label={t("preview.meta.label")}>
          <For each={meta()}>
            {([key, value, auto]) => (
              <>
                <dt>{t(key)}</dt>
                <dd><bdi dir={auto ? "auto" : undefined}>{value}</bdi></dd>
              </>
            )}
          </For>
        </dl>
      </Show>
    </aside>
  );
}

function Pieces(props: { pieces: Piece[] }): JSX.Element {
  return (
    <For each={props.pieces}>
      {(p) => {
        if (p.m !== undefined) return <mark class={p.cls ? `match ${p.cls}` : "match"} data-m={p.m}>{p.text}</mark>;
        return p.cls ? <span class={p.cls}>{p.text}</span> : p.text;
      }}
    </For>
  );
}

function bodyOf<K extends PreviewBody["kind"]>(p: Preview | null, kind: K): Extract<PreviewBody, { kind: K }> | null {
  return p && p.body.kind === kind ? (p.body as Extract<PreviewBody, { kind: K }>) : null;
}

function itemsLabel(count: number): string {
  return tPlural("preview.items", count, formatCount(count, locale()));
}

function typeLabel(b: PreviewBody, ext: string): string {
  if (b.kind === "image") return b.mime;
  if (b.kind === "document") return b.format.toUpperCase();
  if (b.kind === "code") return `${ext ? ext.toUpperCase() : b.language}`;
  return ext ? ext.toUpperCase() : "—";
}

const extOf = (name: string) => {
  const dot = name.lastIndexOf(".");
  return dot > 0 ? name.slice(dot + 1).toLowerCase() : "";
};

const joinPath = (parent: string, name: string) =>
  !parent ? name : /[\\/]$/.test(parent) ? parent + name : `${parent}${parent.includes("/") && !parent.includes("\\") ? "/" : "\\"}${name}`;

/** First index in sorted `items` whose end is after `pos` (binary search). */
function firstEndingAfter<T>(items: T[], end: (x: T) => number, pos: number): number {
  let lo = 0;
  let hi = items.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (end(items[mid]) <= pos) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}

/**
 * Splits `text` into lines (at `\n`), each a list of pieces cut at syntax-span and match
 * boundaries. `spans` and `matches` are sorted, non-overlapping UTF-16 ranges of `text`.
 */
export function splitLines(text: string, spans: SyntaxSpan[], matches: Range16[]): Piece[][] {
  const out: Piece[][] = [];
  let si = 0;
  let mi = 0;
  let from = 0;
  // A trailing newline does not start an empty last line.
  const end = text.endsWith("\n") ? text.length - 1 : text.length;
  while (from <= end) {
    let to = text.indexOf("\n", from);
    if (to < 0 || to > end) to = end;
    si = Math.max(si, firstEndingAfter(spans, (s) => s.end, from));
    mi = Math.max(mi, firstEndingAfter(matches, (m) => m[1], from));
    out.push(linePieces(text, from, to, spans, si, matches, mi));
    from = to + 1;
  }
  return out;
}

function linePieces(text: string, from: number, to: number, spans: SyntaxSpan[], si: number, matches: Range16[], mi: number): Piece[] {
  const cuts = new Set<number>([from, to]);
  for (let i = si; i < spans.length && spans[i].start < to; i++) {
    cuts.add(Math.max(from, spans[i].start));
    cuts.add(Math.min(to, spans[i].end));
  }
  for (let i = mi; i < matches.length && matches[i][0] < to; i++) {
    cuts.add(Math.max(from, matches[i][0]));
    cuts.add(Math.min(to, matches[i][1]));
  }
  const points = [...cuts].sort((a, b) => a - b);
  const pieces: Piece[] = [];
  let s = si;
  let m = mi;
  for (let k = 0; k + 1 < points.length; k++) {
    const a = points[k];
    const b = points[k + 1];
    if (b <= a) continue;
    while (s < spans.length && spans[s].end <= a) s++;
    while (m < matches.length && matches[m][1] <= a) m++;
    const piece: Piece = { text: text.slice(a, b) };
    if (s < spans.length && spans[s].start <= a) piece.cls = `syn-${spans[s].cls}`;
    if (m < matches.length && matches[m][0] <= a) piece.m = m;
    pieces.push(piece);
  }
  return pieces;
}
