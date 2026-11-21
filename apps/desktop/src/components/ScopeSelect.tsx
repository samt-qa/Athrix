import { Show } from "solid-js";
import Segmented, { type SegmentedOption } from "./Segmented";
import { t } from "../i18n";
import { rootName } from "../lib/scope";

type Props = {
  /** The indexed roots (from IndexStatus.roots). */
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
 * caret in the search input.
 */
export default function ScopeSelect(props: Props) {
  const options = (): SegmentedOption<string>[] => [
    { value: "", label: t("scope.wholeLabel"), content: t("scope.whole") },
    ...props.roots.map((root) => {
      const name = rootName(root);
      return {
        value: root,
        label: isDrive(root) ? t("scope.drive", { name }) : t("scope.folder", { path: root }),
        content: <bdi dir="ltr">{name}</bdi>,
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
