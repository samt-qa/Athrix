// "Search file contents too" (tasks 4.7): plain words of every query also look inside files.
// A `content:` filter always searches contents; this switch is for the words without one.
// Remembered in local storage and shared by the main window and the quick-search popup.
import { createSignal } from "solid-js";
import { onSettingChange, readSetting, writeSetting } from "./storage";

const KEY = "searchContent";

const [searchContent, setSignal] = createSignal(readSetting(KEY) === "1");
// Follow a change made in the other window.
onSettingChange(KEY, (v) => setSignal(v === "1"));

export { searchContent };

export function setSearchContent(on: boolean): void {
  setSignal(on);
  writeSetting(KEY, on ? "1" : "0");
}
