import { Show } from "solid-js";
import { t } from "../i18n";

/** Keyboard shortcut hints at the bottom of the panel (design §9.1). In the quick-search popup Esc also closes it. */
export default function HintBar(props: { quick?: boolean }) {
  const ctrl = () => t("keys.ctrl");
  return (
    <ul class="hints" aria-label={t("hints.label")}>
      <li><kbd>↑</kbd><kbd>↓</kbd> {t("hints.navigate")}</li>
      <li><kbd>↵</kbd> {t("hints.open")}</li>
      {/* Not in the popup: its hint bar must stay one line. */}
      <Show when={!props.quick}>
        <li><kbd>{t("keys.tab")}</kbd> {t("hints.complete")}</li>
      </Show>
      <li><bdi dir="ltr"><kbd>{ctrl()}</kbd>+<kbd>↵</kbd></bdi> {t("hints.reveal")}</li>
      <li><bdi dir="ltr"><kbd>{ctrl()}</kbd>+<kbd>C</kbd></bdi> {t("hints.copy")}</li>
      <li><kbd>{t("keys.esc")}</kbd> {t(props.quick ? "hints.clearClose" : "hints.clear")}</li>
    </ul>
  );
}
