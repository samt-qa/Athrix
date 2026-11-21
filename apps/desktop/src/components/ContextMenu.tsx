import { For, onCleanup, onMount, Show } from "solid-js";
import { Portal } from "solid-js/web";

export type MenuItem = {
  label: string;
  /** Shown at the end of the item, e.g. "Ctrl+D". */
  shortcut?: string;
  onSelect: () => void;
};

type Props = {
  /** Viewport position of the pointer (or of the row, when opened from the keyboard). */
  x: number;
  y: number;
  label: string;
  items: MenuItem[];
  onClose: () => void;
};

/**
 * A small context menu (design §10): focus moves to its first item, arrow keys move, Enter
 * selects, Esc / Tab / a click outside closes. Clamped to the window.
 */
export default function ContextMenu(props: Props) {
  let menu!: HTMLDivElement;
  const buttons: HTMLButtonElement[] = [];

  onMount(() => {
    const r = menu.getBoundingClientRect();
    const x = Math.max(8, Math.min(props.x, window.innerWidth - r.width - 8));
    const y = Math.max(8, Math.min(props.y, window.innerHeight - r.height - 8));
    menu.style.left = `${x}px`;
    menu.style.top = `${y}px`;
    menu.style.visibility = "visible";
    buttons[0]?.focus();
    const onDown = (e: MouseEvent) => {
      if (!menu.contains(e.target as Node)) props.onClose();
    };
    const onBlur = () => props.onClose();
    document.addEventListener("mousedown", onDown, true);
    window.addEventListener("blur", onBlur);
    window.addEventListener("resize", onBlur);
    onCleanup(() => {
      document.removeEventListener("mousedown", onDown, true);
      window.removeEventListener("blur", onBlur);
      window.removeEventListener("resize", onBlur);
    });
  });

  function onKeyDown(e: KeyboardEvent) {
    const i = buttons.indexOf(document.activeElement as HTMLButtonElement);
    const n = buttons.length;
    switch (e.key) {
      case "ArrowDown": buttons[(i + 1) % n]?.focus(); break;
      case "ArrowUp": buttons[(i - 1 + n) % n]?.focus(); break;
      case "Home": buttons[0]?.focus(); break;
      case "End": buttons[n - 1]?.focus(); break;
      case "Escape":
      case "Tab":
        props.onClose();
        break;
      default: return;
    }
    e.preventDefault();
    e.stopPropagation();
  }

  return (
    <Portal>
      <div
        ref={menu}
        class="context-menu"
        role="menu"
        aria-label={props.label}
        style={{ visibility: "hidden" }}
        onKeyDown={onKeyDown}
        onContextMenu={(e) => e.preventDefault()}
      >
        <For each={props.items}>
          {(item, i) => (
            <button
              ref={(el) => (buttons[i()] = el)}
              type="button"
              role="menuitem"
              class="context-item"
              tabIndex={-1}
              onClick={() => {
                props.onClose();
                item.onSelect();
              }}
            >
              <span>{item.label}</span>
              <Show when={item.shortcut}>
                <kbd dir="ltr">{item.shortcut}</kbd>
              </Show>
            </button>
          )}
        </For>
      </div>
    </Portal>
  );
}
