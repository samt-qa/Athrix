import { For } from "solid-js";
import { t, type MessageKey } from "../i18n";
import { CHIP_KINDS, type ChipKind } from "../lib/query";

type Props = {
  value: ChipKind;
  onChange: (value: ChipKind) => void;
};

/** Type filter chips (design §10 FilterChip): toggle buttons, exactly one pressed. */
export default function FilterChips(props: Props) {
  return (
    <div class="chips" role="group" aria-label={t("filters.label")}>
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
  );
}
