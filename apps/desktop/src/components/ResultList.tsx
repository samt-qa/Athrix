import { createEffect, createMemo, createSignal, For, on, onCleanup, onMount } from "solid-js";
import type { ResultItem } from "../lib/api";
import ResultRow, { ROW_HEIGHT } from "./ResultRow";

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
  onSelect: (index: number) => void;
  onOpen: (index: number) => void;
  ref?: (handle: ResultListHandle) => void;
};

export const optionId = (listId: string, item: ResultItem) => `${listId}-opt-${item.id}`;

/**
 * Virtualized listbox with fixed 44px rows: only the visible window (+ overscan) is in the DOM,
 * each row absolutely placed with a transform. Selection is owned by the parent and exposed to
 * assistive tech through the input's aria-activedescendant (the list never takes focus).
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

  const range = createMemo(() => {
    const n = props.items.length;
    const start = Math.max(0, Math.floor(scrollTop() / ROW_HEIGHT) - OVERSCAN);
    const end = Math.min(n, Math.ceil((scrollTop() + height()) / ROW_HEIGHT) + OVERSCAN);
    return { start, end };
  });
  props.ref?.({
    pageSize: () => Math.max(1, Math.floor(height() / ROW_HEIGHT) - 1),
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
    const top = i * ROW_HEIGHT;
    const st = viewport.scrollTop;
    if (top < st) scrollTo(top);
    else if (top + ROW_HEIGHT > st + viewport.clientHeight) scrollTo(top + ROW_HEIGHT - viewport.clientHeight);
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
        style={{ height: `${props.items.length * ROW_HEIGHT}px` }}
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
                top={index() * ROW_HEIGHT}
                selected={index() === props.selected}
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

