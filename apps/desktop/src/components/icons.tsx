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

export type StateIconName =
  | "search" | "noResults" | "building" | "notBuilt" | "error" | "stale"
  | "missing" | "locked" | "tooLarge" | "unsupported" | "damaged";

/**
 * Calm line illustration for empty / error states (task 5.5): one glyph per state, drawn in
 * the 24-unit grid of the other icons and shown at 40px with a thinner stroke. Decorative.
 */
export function StateIcon(props: { name: StateIconName; size?: number }) {
  const page = "M7 3.5h6.5l4.5 4.5v11a1.5 1.5 0 0 1-1.5 1.5h-9.5A1.5 1.5 0 0 1 5.5 19V5A1.5 1.5 0 0 1 7 3.5Z";
  const fold = "M13.5 3.5V8H18";
  const glyph = () => {
    switch (props.name) {
      case "search":
        return (<><path d={page} /><path d={fold} /><circle cx="11.5" cy="13" r="2.6" /><path d="m13.5 15 2 2" /></>);
      case "noResults":
        return (<><circle cx="10.5" cy="10.5" r="6.5" /><path d="m20 20-4.8-4.8M8.5 10.5h4" /></>);
      case "building":
        return (<><ellipse cx="12" cy="6" rx="7" ry="2.5" /><path d="M5 6v6c0 1.4 3.1 2.5 7 2.5M19 6v3.5M5 12v6c0 1.4 3.1 2.5 7 2.5" /><circle cx="17.5" cy="16.5" r="3.5" /><path d="M17.5 15v1.5l1 1" /></>);
      case "notBuilt":
        return (<><ellipse cx="12" cy="6" rx="7" ry="2.5" /><path d="M5 6v12c0 1.4 3.1 2.5 7 2.5s7-1.1 7-2.5V6M5 12c0 1.4 3.1 2.5 7 2.5s7-1.1 7-2.5" /></>);
      case "error":
        return (<><path d="M10.3 4.3 3.4 16.5A2 2 0 0 0 5.1 19.5h13.8a2 2 0 0 0 1.7-3L13.7 4.3a2 2 0 0 0-3.4 0Z" /><path d="M12 9.5v4M12 16.5v.01" /></>);
      case "stale":
        return (<><path d="M19.5 12a7.5 7.5 0 1 1-2.2-5.3" /><path d="M19.5 4.5v4h-4" /></>);
      case "missing":
        return (<><path d={page} /><path d={fold} /><path d="m9.5 12 4 4M13.5 12l-4 4" /></>);
      case "locked":
        return (<><rect x="5.5" y="10.5" width="13" height="9" rx="2" /><path d="M8.5 10.5V8a3.5 3.5 0 0 1 7 0v2.5M12 14.5v1.5" /></>);
      case "tooLarge":
        return (<><path d={page} /><path d={fold} /><path d="M11.5 17.5v-6M9 14l2.5-2.5L14 14" /></>);
      case "damaged":
        return (<><path d={page} /><path d={fold} /><path d="m8.5 13 2 1.5 2-2.5 2 2" /></>);
      default:
        return (<><path d={page} /><path d={fold} /><path d="M10 12.2a1.8 1.8 0 1 1 2.6 1.6c-.5.3-.8.7-.8 1.2M11.8 17.2v.01" /></>);
    }
  };
  return (
    <svg class="state-icon" width={props.size ?? 40} height={props.size ?? 40} viewBox="0 0 24 24" {...stroke} stroke-width={1.25}>
      {glyph()}
    </svg>
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

/** A removable (USB) drive: a stick with its plug. */
export function UsbIcon(props: { size?: number }) {
  return (
    <svg class="usb-icon" width={props.size ?? 14} height={props.size ?? 14} viewBox="0 0 24 24" {...stroke}>
      <rect x="7" y="9" width="10" height="13" rx="2" />
      <path d="M9 9V3h6v6M11 5.5v.01M13 5.5v.01" />
    </svg>
  );
}
