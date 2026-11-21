import { createResource, createSignal } from "solid-js";
import { invoke } from "@tauri-apps/api/core";

type AppInfo = { name: string; version: string };

function SearchIcon() {
  return (
    <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor"
      stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <circle cx="11" cy="11" r="7" />
      <path d="m20 20-3.5-3.5" />
    </svg>
  );
}

export default function App() {
  const [info] = createResource(() => invoke<AppInfo>("app_info"));
  const [query, setQuery] = createSignal("");
  // Temporary demo of the UI-to-Rust bridge: how the name index will store the text (task 1.6 replaces it).
  const [normalized] = createResource(query, (q) => invoke<string>("normalize_preview", { text: q }));

  return (
    <div class="shell">
      <header class="topbar">
        <div class="brand">
          <img src="/athrix.svg" alt="" />
          <span class="brand-name">Athrix</span>
          <span class="brand-ar">أثر</span>
        </div>
        <span class="version">{info() ? `v${info()!.version}` : ""}</span>
      </header>

      <main class="main">
        <section class="panel" aria-label="البحث">
          <label class="search">
            <SearchIcon />
            <input
              dir="auto"
              autofocus
              placeholder="ابحث عن ملف، أو محتوى، أو فكرة…"
              value={query()}
              onInput={(e) => setQuery(e.currentTarget.value)}
              onKeyDown={(e) => e.key === "Escape" && setQuery("")}
            />
          </label>
          <div class="progress" />
          <div class="preview">
            <span class="label">هكذا سيخزّنه فهرس الأسماء (تطبيع nass)</span>
            <bdi class="normalized" dir="auto">
              {query()
                ? normalized() ?? ""
                : <span class="empty">اكتب شيئًا، مثل: مُحَاضَرَةُ الفِقْهِ ١</span>}
            </bdi>
            <span class="hint">البحث الفعلي يأتي في المهمة 1.6. هذه الشاشة لاختبار الربط بين الواجهة و Rust.</span>
          </div>
        </section>
      </main>

      <footer class="statusbar">
        <span>الفهرس: لم يبدأ بعد</span>
        <span><kbd>Esc</kbd> مسح</span>
      </footer>
    </div>
  );
}
