import { invoke, isTauri } from "@tauri-apps/api/core";

/** Calls a Rust command; resolves to null outside Tauri or when the command fails. */
export async function safeInvoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T | null> {
  if (!isTauri()) return null;
  try {
    return await invoke<T>(cmd, args);
  } catch (err) {
    console.error(`invoke("${cmd}") failed`, err);
    return null;
  }
}
