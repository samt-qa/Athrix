/* @refresh reload */
// Settings first: these modules apply the saved lang/dir and theme to <html> on import, before the first render.
import "./i18n";
import "./lib/theme";
import { render } from "solid-js/web";
import "./styles/fonts.css";
import "./styles/tokens.css";
import "./styles/app.css";
import App from "./App";

render(() => <App />, document.getElementById("root") as HTMLElement);
