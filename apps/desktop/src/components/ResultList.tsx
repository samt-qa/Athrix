import { createEffect, createMemo, createSignal, For, on, onCleanup, onMount } from "solid-js";
import type { ResultItem } from "../lib/api";
import ResultRow, { ROW_HEIGHT, rowHeight } from "./ResultRow";

/** Rows rendered above and below the viewport. */
const OVERSCAN = 6;

export type ResultListHandle = {
  /** Rows that fit in the viewport (for PageUp/PageDown). */
  pageSize: () => number;
  /** Whether row `index` is in the DOM (aria-activedescendant must point at a real node). */
  isRendered: (index: number) => boolean;
};

type Props = {
  id: string;
  label: string;
  items: ResultItem[];
  selected: number;
  now: number;
  /** Quick-search popup: every row 44px, a snippet replaces the path. */
  compact?: boolean;
  onSelect: (index: number) => void;
  onOpen: (index: number) => void;
  /** Total height of all rows, in px (the popup sizes its list area with it). */
  onHeight?: (px: number) => void;
  ref?: (handle: ResultListHandle) => void;
};

export const optionId = (listId: string, item: ResultItem) => `${listId}-opt-${item.id}`;

/** Index of the last row whose top is at or above `y` (binary search over sorted `tops`). */
function rowAt(tops: number[], y: number): number {
  let lo = 0;
  let hi = tops.length - 1;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (tops[mid] <= y) lo = mid;
    else hi = mid - 1;
  }
  return lo;
}

/**
 * Virtualized listbox with fixed-height rows: only the visible window (+ overscan) is in the
 * DOM, each row absolutely placed with a transform. Rows have one of two fixed heights, known
 * from the data (44px; 64px for a content match with a snippet in the main window), so the
 * offsets are a prefix sum computed once per result set and nothing is measured. Selection is
 * owned by the parent and exposed to assistive tech through the input's aria-activedescendant
 * (the list never takes focus).
 */
export default function ResultList(props: Props) {
  let viewport!: HTMLDivElement;
  const [scrollTop, setScrollTop] = createSignal(0);
  const [height, setHeight] = createSignal(0);

  onMount(() => {
    const ro = new ResizeObserver(() => setHeight(viewport.clientHeight));
    ro.observe(viewport);
    setHeight(viewport.clientHeight);
    onCleanup(() => ro.disconnect());
  });

  /** Top of each row, plus the total height as the last element. */
  const tops = createMemo(() => {
    const items = props.items;
    const out = new Array<number>(items.length + 1);
    let y = 0;
    for (let i = 0; i < items.length; i++) {
      out[i] = y;
      y += rowHeight(items[i], !!props.compact);
    }
    out[items.length] = y;
    return out;
  });
  const total = () => tops()[props.items.length];
  const topOf = (i: number) => tops()[i] ?? 0;
  const heightOf = (i: number) => topOf(i + 1) - topOf(i);
  createEffect(() => props.onHeight?.(total()));

  const range = createMemo(() => {
    const n = props.items.length;
    if (!n) return { start: 0, end: 0 };
    const t = tops();
    const start = Math.max(0, rowAt(t, scrollTop()) - OVERSCAN);
    const end = Math.min(n, rowAt(t, scrollTop() + height()) + 1 + OVERSCAN);
    return { start, end };
  });
  props.ref?.({
    // Average row height: PageUp/PageDown move by about a screenful either way.
    pageSize: () => Math.max(1, Math.floor(height() / (props.items.length ? total() / props.items.length : ROW_HEIGHT)) - 1),
    isRendered: (i) => i >= range().start && i < range().end,
  });

  // Slicing keeps item identity, so <For> reuses rows that stay in view while scrolling.
  const visible = createMemo(() => props.items.slice(range().start, range().end));

  const scrollTo = (top: number) => {
    viewport.scrollTop = top;
    setScrollTop(viewport.scrollTop);
  };

  // New result set: back to the top.
  createEffect(on(() => props.items, () => scrollTo(0), { defer: true }));

  // Keep the selected row in view (keyboard navigation).
  createEffect(on(() => props.selected, (i) => {
    const top = topOf(i);
    const bottom = top + heightOf(i);
    const st = viewport.scrollTop;
    if (top < st) scrollTo(top);
    else if (bottom > st + viewport.clientHeight) scrollTo(bottom - viewport.clientHeight);
  }, { defer: true }));

  return (
    <div
      ref={viewport}
      class="results"
      classList={{ empty: !props.items.length }}
      onScroll={() => setScrollTop(viewport.scrollTop)}
    >
      <div
        id={props.id}
        role="listbox"
        aria-label={props.label}
        class="results-canvas"
        style={{ height: `${total()}px` }}
      >
        <For each={visible()}>
          {(item, i) => {
            const index = () => range().start + i();
            return (
              <ResultRow
                id={optionId(props.id, item)}
                item={item}
                index={index()}
                setSize={props.items.length}
                top={topOf(index())}
                selected={index() === props.selected}
                compact={!!props.compact}
                now={props.now}
                onSelect={() => props.onSelect(index())}
                onOpen={() => props.onOpen(index())}
              />
            );
          }}
        </For>
      </div>
    </div>
  );
}
