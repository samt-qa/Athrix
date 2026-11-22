import { createSignal, For, onMount, Show } from "solid-js";
import { t } from "../i18n";
import { api, errorMessage, type McpServerPath, type McpSettings } from "../lib/api";
import { SwitchRow } from "./SettingsDialog";

/** The Claude Desktop `claude_desktop_config.json` entry for the server at `path`. */
export function claudeDesktopSnippet(path: string): string {
  return JSON.stringify({ mcpServers: { athrix: { command: path } } }, null, 2);
}

/**
 * Settings → MCP (task 7.2): the `athrix-mcp` server that AI assistants start to search the
 * user's files. Off by default; read-only; only the folders listed here are visible to it.
 * Every change is saved at once (`set_mcp_settings`); the server reads it on its next call.
 */
export default function McpSection(props: { onNotice: (message: string) => void }) {
  const [settings, setSettings] = createSignal<McpSettings | null>(null);
  const [server, setServer] = createSignal<McpServerPath | null>(null);
  const [busy, setBusy] = createSignal(false);

  onMount(() => {
    api.getMcpSettings().then(setSettings, (err) => console.warn("MCP settings unavailable", err));
    api.mcpServerPath().then(setServer, (err) => console.warn("MCP server path unavailable", err));
  });

  async function save(change: Partial<McpSettings>) {
    const current = settings();
    if (!current) return;
    setBusy(true);
    try {
      setSettings(await api.setMcpSettings({ ...current, ...change }));
    } catch (err) {
      props.onNotice(t("mcp.failed", { message: errorMessage(err) }));
    } finally {
      setBusy(false);
    }
  }

  async function addFolder() {
    const current = settings();
    if (!current) return;
    try {
      const folder = await api.mcpPickFolder();
      if (folder) await save({ allowedFolders: [...current.allowedFolders, folder] });
    } catch (err) {
      props.onNotice(t("mcp.failed", { message: errorMessage(err) }));
    }
  }

  async function copyConfig() {
    const s = server();
    if (!s) return;
    try {
      await navigator.clipboard.writeText(claudeDesktopSnippet(s.path));
      props.onNotice(t("mcp.copied"));
    } catch (err) {
      props.onNotice(t("mcp.failed", { message: errorMessage(err) }));
    }
  }

  return (
    <section class="settings-group" aria-labelledby="set-mcp">
      <h3 id="set-mcp" class="settings-group-title">{t("mcp.title")}</h3>
      <p class="settings-hint">{t("mcp.intro")}</p>
      <Show when={settings()} fallback={<p class="settings-hint">{t("mcp.loading")}</p>}>
        {(s) => (
          <>
            <SwitchRow
              checked={s().enabled}
              disabled={busy()}
              onChange={(on) => void save({ enabled: on })}
              label={t("mcp.enable")}
              hint={t("mcp.enableWarning")}
            />
            <Show when={s().enabled && s().allowedFolders.length === 0}>
              <p class="settings-hint mcp-attention" role="status">{t("mcp.noFolders")}</p>
            </Show>

            <h4 class="mcp-subtitle" id="mcp-folders">{t("mcp.folders")}</h4>
            <p class="settings-hint">{t("mcp.foldersHint")}</p>
            <ul class="mcp-folders" aria-labelledby="mcp-folders">
              <For each={s().allowedFolders} fallback={<li class="mcp-folder-empty">{t("mcp.foldersEmpty")}</li>}>
                {(folder) => (
                  <li class="mcp-folder">
                    <bdi dir="ltr" class="mcp-folder-path" title={folder}>{folder}</bdi>
                    <button
                      type="button"
                      class="btn-ghost"
                      disabled={busy()}
                      aria-label={t("mcp.removeFolderLabel", { folder })}
                      onClick={() => void save({ allowedFolders: s().allowedFolders.filter((f) => f !== folder) })}
                    >
                      {t("mcp.removeFolder")}
                    </button>
                  </li>
                )}
              </For>
            </ul>
            <div class="settings-inline">
              <button type="button" class="btn-secondary" disabled={busy()} onClick={() => void addFolder()}>
                {t("mcp.addFolder")}
              </button>
            </div>

            <SwitchRow
              checked={s().allowContentRead}
              disabled={busy()}
              onChange={(on) => void save({ allowContentRead: on })}
              label={t("mcp.allowRead")}
              hint={t("mcp.allowReadHint")}
            />

            <h4 class="mcp-subtitle">{t("mcp.connect")}</h4>
            <p class="settings-hint">{t("mcp.connectHint")}</p>
            <div class="settings-inline">
              <button type="button" class="btn-secondary" disabled={!server()} onClick={() => void copyConfig()}>
                {t("mcp.copyConfig")}
              </button>
              <Show when={server()}>
                {(p) => (
                  <span class="settings-status">
                    <Show when={p().exists} fallback={t("mcp.serverMissing")}>
                      <bdi dir="ltr">{p().path}</bdi>
                    </Show>
                  </span>
                )}
              </Show>
            </div>
          </>
        )}
      </Show>
    </section>
  );
}
