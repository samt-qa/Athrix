import { createMemo, Index, type JSX } from "solid-js";

export type SegmentedOption<T extends string> = {
  value: T;
  /** Accessible name; also shown as the tooltip. */
  label: string;
  /** Visible content (text or icon). Defaults to `label`. */
  content?: JSX.Element;
  /** Language of the visible content, e.g. "ar" for "ع". */
  lang?: string;
};

type Props<T extends string> = {
  label: string;
  value: T;
  options: SegmentedOption<T>[];
  onChange: (value: T) => void;
};

/** Small radiogroup of ghost buttons (roving tabindex, arrow keys follow the reading direction). */
export default function Segmented<T extends string>(props: Props<T>) {
  const buttons: HTMLButtonElement[] = [];
  // Memoized so key handlers (which run outside the reactive owner) don't re-run the options getter and recreate JSX.
  const options = createMemo(() => props.options);

  const move = (from: number, delta: number) => {
    const n = options().length;
    const next = (from + delta + n) % n;
    props.onChange(options()[next].value);
    buttons[next]?.focus();
  };

  const onKeyDown = (e: KeyboardEvent, index: number) => {
    const rtl = getComputedStyle(e.currentTarget as Element).direction === "rtl";
    switch (e.key) {
      case "ArrowRight": move(index, rtl ? -1 : 1); break;
      case "ArrowLeft": move(index, rtl ? 1 : -1); break;
      case "ArrowDown": move(index, 1); break;
      case "ArrowUp": move(index, -1); break;
      case "Home": move(0, 0); break;
      case "End": move(options().length - 1, 0); break;
      default: return;
    }
    e.preventDefault();
  };

  return (
    <div class="segmented" role="radiogroup" aria-label={props.label}>
      {/* Index (not For) keeps the same buttons when labels change, so focus survives a language switch. */}
      <Index each={options()}>
        {(opt, i) => (
          <button
            ref={(el) => (buttons[i] = el)}
            type="button"
            role="radio"
            class="segmented-btn"
            aria-checked={opt().value === props.value}
            aria-label={opt().label}
            title={opt().label}
            lang={opt().lang}
            tabIndex={opt().value === props.value ? 0 : -1}
            onClick={() => props.onChange(opt().value)}
            onKeyDown={(e) => onKeyDown(e, i)}
          >
            {opt().content ?? opt().label}
          </button>
        )}
      </Index>
    </div>
  );
}
