import { createSignal, onCleanup, onMount, Show } from "solid-js";
import Segmented, { type SegmentedOption } from "./Segmented";
import { UsbIcon } from "./icons";
import { t } from "../i18n";
import { api, onVolumesChanged, type Volume } from "../lib/api";
import { rootKey, rootName } from "../lib/scope";

type Props = {
  /** The indexed roots that can be searched now (IndexStatus.roots without the offline ones). */
  roots: string[];
  /** "" = whole computer, else one of `roots`. */
  value: string;
  onChange: (scope: string) => void;
};

const isDrive = (path: string) => /^[a-z]:[\\/]?$/i.test(path);

/**
 * Where to search, chosen before typing: the whole computer or one indexed drive (design §10
 * SearchBar addon). A radiogroup (roving tabindex, arrows follow the reading direction). Hidden
 * when only one root is indexed, since then both choices are the same. Mouse clicks keep the
 * caret in the search input. Roots on a removable drive carry a USB icon; the list of drives
 * follows `volumes://changed` (the owner drops roots whose drive is unplugged).
 */
export default function ScopeSelect(props: Props) {
  const [volumes, setVolumes] = createSignal<Volume[]>([]);

  onMount(() => {
    let stop: (() => void) | undefined;
    let disposed = false;
    api.listVolumes().then(setVolumes, () => {});
    void onVolumesChanged(setVolumes).then((un) => (disposed ? un() : (stop = un)));
    onCleanup(() => {
      disposed = true;
      stop?.();
    });
  });

  /** Whether `root` lies on a removable drive. */
  const onUsb = (root: string) => {
    const key = rootKey(root);
    return volumes().some((v) => {
      if (v.kind !== "removable") return false;
      const vk = rootKey(v.path);
      return key === vk || key.startsWith(`${vk}/`);
    });
  };

  const options = (): SegmentedOption<string>[] => [
    { value: "", label: t("scope.wholeLabel"), content: t("scope.whole") },
    ...props.roots.map((root) => {
      const name = rootName(root);
      const usb = onUsb(root);
      const label = isDrive(root) ? t("scope.drive", { name }) : t("scope.folder", { path: root });
      return {
        value: root,
        label: usb ? t("scope.removable", { name: label }) : label,
        content: (
          <span class="scope-option">
            <Show when={usb}>
              <UsbIcon size={12} />
            </Show>
            <bdi dir="ltr">{name}</bdi>
          </span>
        ),
      };
    }),
  ];

  return (
    <Show when={props.roots.length > 1}>
      <div class="scope-select" onMouseDown={(e) => e.preventDefault()}>
        <Segmented<string> label={t("scope.label")} value={props.value} options={options()} onChange={props.onChange} />
      </div>
    </Show>
  );
}
