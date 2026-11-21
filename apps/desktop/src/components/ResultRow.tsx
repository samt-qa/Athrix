import { createMemo, For, Show } from "solid-js";
import { locale, t } from "../i18n";
import type { ResultItem } from "../lib/api";
import { categoryOf } from "../lib/fileKinds";
import {
  formatDateTime, formatRelative, formatSize, highlightParts, splitName, splitPath, type TextPart,
} from "../lib/format";
import { TypeIcon } from "./icons";

export const ROW_HEIGHT = 44;

type Props = {
  id: string;
  item: ResultItem;
  index: number;
  setSize: number;
  top: number;
  selected: boolean;
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
 * One 44px result row (design §10 ResultRow). The name gets the direction of its first strong
 * character and the extension is isolated from the stem, so digits at the end of an Arabic stem
 * never get pulled into the extension ("تقرير 2026.pdf" does not render as "pdf.2026 تقرير");
 * in an RTL name the extension sits at the left with its dot next to the stem, as in Explorer.
 * The parent path is LTR with every segment in its own <bdi>, truncated from the middle (the
 * last folder always stays visible).
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

  return (
    <div
      id={props.id}
      role="option"
      class="row"
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
        </span>
        <bdi class="row-path" dir="ltr" title={props.item.parent}>
          <span class="row-path-head">
            <For each={path().head}>{(s) => <><bdi>{s.text}</bdi>{s.sep}</>}</For>
          </span>
          <Show when={path().tail}>{(s) => <bdi class="row-path-tail">{s().text}</bdi>}</Show>
        </bdi>
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
