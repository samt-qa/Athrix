import type { FileCategory } from "../lib/fileKinds";

const stroke = {
  fill: "none", stroke: "currentColor", "stroke-width": 1.75, "stroke-linecap": "round",
  "stroke-linejoin": "round", "aria-hidden": true,
} as const;

export function SearchIcon() {
  return (
    <svg width="20" height="20" viewBox="0 0 24 24" {...stroke}>
      <circle cx="11" cy="11" r="7" />
      <path d="m20 20-3.5-3.5" />
    </svg>
  );
}

export function AlertIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" {...stroke}>
      <circle cx="12" cy="12" r="9" />
      <path d="M12 7.5v5.5M12 16.5v.01" />
    </svg>
  );
}

/** Per-category glyph inside the 20px type badge (design §2.5 colors via --type-* tokens). */
export function TypeIcon(props: { category: FileCategory }) {
  const glyph = () => {
    switch (props.category) {
      case "folder":
        return <path d="M3.5 7.5a2 2 0 0 1 2-2h3.6l2 2.2h7.4a2 2 0 0 1 2 2v7.8a2 2 0 0 1-2 2h-13a2 2 0 0 1-2-2Z" />;
      case "image":
        return (<><rect x="4" y="5" width="16" height="14" rx="2" /><circle cx="9" cy="10" r="1.6" /><path d="m5 18 5-5 3 3 2-2 4 4" /></>);
      case "media":
        return (<><rect x="4" y="6" width="16" height="12" rx="2" /><path d="m10.5 9.5 4 2.5-4 2.5Z" /></>);
      case "code":
        return <path d="m9 8-4 4 4 4M15 8l4 4-4 4" />;
      case "archive":
        return (<><rect x="4" y="5" width="16" height="14" rx="2" /><path d="M12 5v2M12 9v2M12 13v2" /></>);
      case "sheet":
        return (<><rect x="4.5" y="4.5" width="15" height="15" rx="2" /><path d="M4.5 10h15M4.5 15h15M10 4.5v15" /></>);
      case "slides":
        return (<><rect x="3.5" y="5" width="17" height="11" rx="1.5" /><path d="M12 16v3.5M9 19.5h6" /></>);
      default:
        return (<><path d="M7 3.5h6.5l4.5 4.5v11a1.5 1.5 0 0 1-1.5 1.5h-9.5A1.5 1.5 0 0 1 5.5 19V5A1.5 1.5 0 0 1 7 3.5Z" /><path d="M13.5 3.5V8H18M8.5 13h7M8.5 16.5h5" /></>);
    }
  };
  return (
    <span class={`type-badge type-${props.category}`}>
      <svg width="14" height="14" viewBox="0 0 24 24" {...stroke}>{glyph()}</svg>
    </span>
  );
}

/** "Matched inside the file": a page with text lines under a small lens. */
export function ContentMatchIcon() {
  return (
    <svg width="12" height="12" viewBox="0 0 24 24" {...stroke}>
      <path d="M4 5h16M4 10h9M4 15h6" />
      <circle cx="16.5" cy="16.5" r="3.5" />
      <path d="m19 19 2.5 2.5" />
    </svg>
  );
}
