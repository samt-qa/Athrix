import { t } from "../i18n";

/** Keyboard shortcut hints at the bottom of the panel (design §9.1). */
export default function HintBar() {
  const ctrl = () => t("keys.ctrl");
  return (
    <ul class="hints" aria-label={t("hints.label")}>
      <li><kbd>↑</kbd><kbd>↓</kbd> {t("hints.navigate")}</li>
      <li><kbd>↵</kbd> {t("hints.open")}</li>
      <li><bdi dir="ltr"><kbd>{ctrl()}</kbd>+<kbd>↵</kbd></bdi> {t("hints.reveal")}</li>
      <li><bdi dir="ltr"><kbd>{ctrl()}</kbd>+<kbd>C</kbd></bdi> {t("hints.copy")}</li>
      <li><kbd>{t("keys.esc")}</kbd> {t("hints.clear")}</li>
    </ul>
  );
}
