import { onCleanup, onMount, type JSX } from "solid-js";
import { Portal } from "solid-js/web";

type Props = {
  /** id of the element that names the dialog (aria-labelledby). */
  labelledBy: string;
  describedBy?: string;
  /** Esc, or a click on the backdrop. */
  onClose: () => void;
  /** Extra class on the dialog box. */
  class?: string;
  /** Element to focus first; defaults to the first focusable one. */
  initialFocus?: () => HTMLElement | undefined;
  /** Backdrop click closes (default true). */
  closeOnBackdrop?: boolean;
  children: JSX.Element;
};

const FOCUSABLE =
  'a[href], button:not(:disabled), input:not(:disabled), select:not(:disabled), textarea:not(:disabled), [tabindex]:not([tabindex="-1"])';

/** Focusable, visible elements inside `root`, in tab order. */
export function focusables(root: HTMLElement): HTMLElement[] {
  return [...root.querySelectorAll<HTMLElement>(FOCUSABLE)].filter((el) => el.offsetParent !== null || el === document.activeElement);
}

/**
 * A modal dialog over the whole window: focus moves in on open, Tab stays inside, Esc closes,
 * and focus goes back to where it was on close.
 */
export default function Modal(props: Props) {
  let box!: HTMLDivElement;
  const before = document.activeElement as HTMLElement | null;

  onMount(() => {
    const first = props.initialFocus?.() ?? focusables(box)[0] ?? box;
    first.focus();
  });
  onCleanup(() => {
    if (before?.isConnected) before.focus();
  });

  function onKeyDown(e: KeyboardEvent) {
    if (e.key === "Escape" && !e.isComposing) {
      e.preventDefault();
      e.stopPropagation();
      props.onClose();
      return;
    }
    if (e.key !== "Tab") return;
    const list = focusables(box);
    if (!list.length) {
      e.preventDefault();
      return;
    }
    const first = list[0];
    const last = list[list.length - 1];
    const active = document.activeElement;
    if (e.shiftKey && (active === first || !box.contains(active))) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && (active === last || !box.contains(active))) {
      e.preventDefault();
      first.focus();
    }
  }

  return (
    <Portal>
      <div
        class="modal-backdrop"
        onMouseDown={(e) => {
          if (e.target === e.currentTarget && props.closeOnBackdrop !== false) props.onClose();
        }}
      >
        <div
          ref={box}
          class={`modal ${props.class ?? ""}`}
          role="dialog"
          aria-modal="true"
          aria-labelledby={props.labelledBy}
          aria-describedby={props.describedBy}
          tabIndex={-1}
          onKeyDown={onKeyDown}
        >
          {props.children}
        </div>
      </div>
    </Portal>
  );
}
