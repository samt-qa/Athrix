/* @refresh reload */
// Settings first: these modules apply the saved lang/dir and theme to <html> on import, before the first render.
import { locale } from "./i18n";
import "./lib/theme";
import { createEffect, createRoot } from "solid-js";
import { render } from "solid-js/web";
import "./styles/fonts.css";
import "./styles/tokens.css";
import "./styles/app.css";
import { announceLocale, isQuickWindow } from "./lib/window";
import App from "./App";

// The popup window is transparent around its rounded panel (styles: `html.quick`).
if (isQuickWindow) document.documentElement.classList.add("quick");

// Keep the tray menu in the UI language (at start-up and on every switch).
createRoot(() => createEffect(() => announceLocale(locale())));

render(() => <App mode={isQuickWindow ? "quick" : "main"} />, document.getElementById("root") as HTMLElement);
