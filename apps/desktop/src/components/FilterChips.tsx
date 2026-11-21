import { For, Show } from "solid-js";
import { t, type MessageKey } from "../i18n";
import { CHIP_KINDS, type ChipKind } from "../lib/query";
import { ContentMatchIcon } from "./icons";

type Props = {
  value: ChipKind;
  onChange: (value: ChipKind) => void;
  /** "Search file contents too" (shown at the inline end, apart from the type chips). */
  content?: boolean;
  onContentChange?: (on: boolean) => void;
};

/**
 * Type filter chips (design §10 FilterChip): toggle buttons, exactly one pressed. At the inline
 * end, apart from them, an independent toggle: plain words search file contents too.
 */
export default function FilterChips(props: Props) {
  return (
    <div class="chips">
      <div class="chips-group" role="group" aria-label={t("filters.label")}>
        <For each={CHIP_KINDS}>
          {(kind) => (
            <button
              type="button"
              class="chip"
              aria-pressed={props.value === kind}
              // Keep the caret in the search input when a chip is clicked.
              onMouseDown={(e) => e.preventDefault()}
              onClick={() => props.onChange(kind)}
            >
              {t(`filters.${kind}` as MessageKey)}
            </button>
          )}
        </For>
      </div>
      <Show when={props.onContentChange}>
        <button
          type="button"
          class="chip chip-content"
          aria-pressed={!!props.content}
          title={t("content.searchToo")}
          onMouseDown={(e) => e.preventDefault()}
          onClick={() => props.onContentChange?.(!props.content)}
        >
          <ContentMatchIcon />
          {t("filters.content")}
        </button>
      </Show>
    </div>
  );
}
