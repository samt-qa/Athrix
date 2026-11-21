import { createMemo, For, Show } from "solid-js";
import { locale, t } from "../i18n";
import type { ResultItem } from "../lib/api";
import { categoryOf } from "../lib/fileKinds";
import {
  formatDateTime, formatRelative, formatSize, highlightParts, snippetParts, splitName, splitPath, type TextPart,
} from "../lib/format";
import { ContentMatchIcon, TypeIcon } from "./icons";

/** A row with two lines: name + path (design §5, "comfortable"). */
export const ROW_HEIGHT = 44;
/** A content match in the main window: name + path + snippet line. */
export const SNIPPET_ROW_HEIGHT = 64;

/**
 * Fixed height of a row, from its data alone (the virtual list places rows without measuring).
 * In the main window a row with a content snippet gets a third line, so the path stays visible;
 * the compact popup keeps every row at 44px and shows the snippet instead of the path, as in
 * the design's popup mockup (§9.1).
 */
export function rowHeight(item: ResultItem, compact: boolean): number {
  return !compact && item.snippet ? SNIPPET_ROW_HEIGHT : ROW_HEIGHT;
}

type Props = {
  id: string;
  item: ResultItem;
  index: number;
  setSize: number;
  top: number;
  selected: boolean;
  /** Quick-search popup: the snippet, if any, replaces the path line. */
  compact: boolean;
  /** Re-read every minute so relative times stay fresh. */
  now: number;
  onSelect: () => void;
  onOpen: () => void;
};

const RTL_CHAR = /[֐-ࣿיִ-﷿ﹰ-﻿]/;
const LTR_CHAR = /[A-Za-zÀ-ɏͰ-ϿЀ-ӿ]/;

/** Direction of the first strong character (what dir="auto" would pick). */
function firstStrongDir(s: string): "rtl" | "ltr" {
  for (const c of s) {
    if (RTL_CHAR.test(c)) return "rtl";
    if (LTR_CHAR.test(c)) return "ltr";
  }
  return "ltr";
}

function Parts(props: { parts: TextPart[] }) {
  return (
    <For each={props.parts}>
      {(p) => (p.match ? <mark class="match">{p.text}</mark> : p.text)}
    </For>
  );
}

/**
 * One result row (design §10 ResultRow): 44px, or 64px with a content snippet in the main
 * window. The name gets the direction of its first strong character and the extension is
 * isolated from the stem, so digits at the end of an Arabic stem never get pulled into the extension ("تقرير 2026.pdf" does not render as "pdf.2026 تقرير");
 * in an RTL name the extension sits at the left with its dot next to the stem, as in Explorer.
 * The parent path is LTR with every segment in its own <bdi>, truncated from the middle (the
 * last folder always stays visible). A content match adds a snippet line (dir="auto", matched
 * words marked) and a small badge after the name saying the content matched.
 */
export default function ResultRow(props: Props) {
  const name = createMemo(() => {
    const it = props.item;
    const [stem, dotExt] = splitName(it.name, it.ext, it.isDir);
    return {
      dir: firstStrongDir(stem),
      stem: highlightParts(it.name, it.highlights, 0, stem.length),
      ext: dotExt ? highlightParts(it.name, it.highlights, stem.length, it.name.length) : null,
    };
  });
  const path = createMemo(() => {
    const segs = splitPath(props.item.parent);
    return { head: segs.slice(0, -1), tail: segs[segs.length - 1] };
  });

  const snippet = createMemo(() => {
    const sn = props.item.snippet;
    return sn ? snippetParts(sn.text, sn.highlights) : null;
  });
  const source = () => props.item.matched ?? "name";
  const sourceLabel = () => (source() === "both" ? t("results.matchedBoth") : t("results.matchedContent"));
  const showPath = () => !(props.compact && snippet());

  return (
    <div
      id={props.id}
      role="option"
      class="row"
      classList={{ "row-tall": !props.compact && !!snippet() }}
      aria-selected={props.selected}
      aria-setsize={props.setSize}
      aria-posinset={props.index + 1}
      style={{ transform: `translateY(${props.top}px)` }}
      // Keep focus in the search input: the list is driven through aria-activedescendant.
      onMouseDown={(e) => e.preventDefault()}
      onClick={props.onSelect}
      onDblClick={props.onOpen}
    >
      <TypeIcon category={categoryOf(props.item.ext, props.item.isDir)} />
      <div class="row-main">
        <span class="row-name" dir={name().dir} title={props.item.name}>
          <bdi class="row-stem"><Parts parts={name().stem} /></bdi>
          <Show when={name().ext}>{(ext) => <bdi class="row-ext" dir={name().dir}><Parts parts={ext()} /></bdi>}</Show>
          {/* Where the match came from: only marked when the content matched (name is the default). */}
          <Show when={source() !== "name"}>
            <span class="row-source" classList={{ both: source() === "both" }} role="img"
              aria-label={sourceLabel()} title={sourceLabel()}>
              <ContentMatchIcon />
            </span>
          </Show>
        </span>
        <Show when={showPath()}>
          <bdi class="row-path" dir="ltr" title={props.item.parent}>
            <span class="row-path-head">
              <For each={path().head}>{(s) => <><bdi>{s.text}</bdi>{s.sep}</>}</For>
            </span>
            <Show when={path().tail}>{(s) => <bdi class="row-path-tail">{s().text}</bdi>}</Show>
          </bdi>
        </Show>
        <Show when={snippet()}>
          {(parts) => (
            // Its own direction (the excerpt may be English in an Arabic UI); one line, ellipsis.
            <span class="row-snippet" dir="auto" title={props.compact ? props.item.parent : undefined}>
              <Parts parts={parts()} />
            </span>
          )}
        </Show>
      </div>
      <div class="row-meta">
        {/* Localized sizes may contain Arabic unit names, so they get dir=auto (bdi default), not ltr. */}
        <bdi class="row-size">{props.item.size !== null ? formatSize(props.item.size, locale()) : ""}</bdi>
        <Show when={props.item.modified}>
          {(m) => (
            <span class="row-time" title={t("results.modified", { date: formatDateTime(m(), locale()) })}>
              {formatRelative(m(), locale(), props.now)}
            </span>
          )}
        </Show>
      </div>
    </div>
  );
}
