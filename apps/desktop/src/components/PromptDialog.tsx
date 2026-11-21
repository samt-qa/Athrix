import { createSignal, createUniqueId, Show } from "solid-js";
import { t } from "../i18n";
import { errorMessage } from "../lib/api";
import Modal from "./Modal";

type Props = {
  title: string;
  label: string;
  /** Shown under the title (e.g. the query being saved). */
  detail?: string;
  initial: string;
  confirm: string;
  /** Resolves when done (the dialog then closes); a rejection is shown in the dialog. */
  onSubmit: (value: string) => Promise<unknown>;
  onClose: () => void;
};

/** A small "name it" dialog: one text field, confirm / cancel. Enter confirms, Esc cancels. */
export default function PromptDialog(props: Props) {
  const id = createUniqueId();
  const [value, setValue] = createSignal(props.initial);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  let input!: HTMLInputElement;

  async function submit(e: Event) {
    e.preventDefault();
    const v = value().trim();
    if (!v || busy()) return;
    setBusy(true);
    setError(null);
    try {
      await props.onSubmit(v);
      props.onClose();
    } catch (err) {
      setError(errorMessage(err));
      setBusy(false);
    }
  }

  return (
    <Modal
      labelledBy={`${id}-title`}
      describedBy={props.detail ? `${id}-detail` : undefined}
      onClose={props.onClose}
      class="prompt"
      initialFocus={() => {
        queueMicrotask(() => input.select());
        return input;
      }}
    >
      <form onSubmit={submit}>
        <h2 id={`${id}-title`} class="modal-title">{props.title}</h2>
        <Show when={props.detail}>
          <p id={`${id}-detail`} class="prompt-detail"><bdi dir="auto">{props.detail}</bdi></p>
        </Show>
        <label class="field">
          <span class="field-label">{props.label}</span>
          <input
            ref={input}
            class="text-input"
            type="text"
            dir="auto"
            maxLength={120}
            value={value()}
            onInput={(e) => setValue(e.currentTarget.value)}
            aria-invalid={error() ? "true" : undefined}
          />
        </label>
        <Show when={error()}>
          <p class="field-error" role="alert">{t("library.actionFailed", { message: error()! })}</p>
        </Show>
        <div class="modal-actions">
          <button type="button" class="btn-secondary" onClick={props.onClose}>{t("dialog.cancel")}</button>
          <button type="submit" class="btn-primary" disabled={!value().trim() || busy()}>{props.confirm}</button>
        </div>
      </form>
    </Modal>
  );
}
