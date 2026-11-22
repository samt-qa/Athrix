# خادم MCP في أثر — المساعدات الذكية

**بروتوكول سياق النماذج (Model Context Protocol, MCP)** معيار مفتوح تتصل به المساعدات الذكية (Claude Desktop وClaude Code
وغيرهما) ببرامج على جهازك. يأتي أثر ببرنامج صغير اسمه `athrix-mcp` يتيح للمساعد أن **يبحث** في ملفاتك عبر فهارس أثر:
«أين عقد الإيجار الذي وقّعته في 2024؟»، «ما ملفات PDF في مجلد المشروع التي تذكر الميزانية؟».

## نموذج الخصوصية

- **مطفأ افتراضيًا.** لا يجيب الخادم عن أي أداة حتى تفعّله من أثر ← الإعدادات ← **المساعدات الذكية** وتضيف مجلدًا واحدًا
  على الأقل. وقبل ذلك يرد على المساعد برسالة تخبرك أين تفعّله.
- **المجلدات المسموح بها فقط.** لا يرى المساعد إلا ما داخل المجلدات التي أضفتها. يُفحص كل مسار في كل نتيجة وكل قراءة:
  يُطبَّع، ويُقارن دون حساسية لحالة الأحرف على ويندوز، ويُحَلّ `..`، وتُرفض مسارات الشبكة (UNC) ومسارات الأجهزة و`\\?\`
  وتدفقات البيانات البديلة (`file:stream`)، ثم يُحَلّ المسار الحقيقي على القرص (الروابط الرمزية ونقاط الوصل junction)
  ويُفحص من جديد، فرابط داخل مجلد مسموح يشير إلى خارجه مرفوض.
- **للقراءة فقط.** أربع أدوات للبحث والقراءة؛ لا توجد أداة تكتب أو تحذف أو تنقل أو تفتح أو تشغّل ملفًا. والفهارس تُفتح
  للقراءة فقط.
- **قراءة النص اختيارية.** أداة `read_text` (نص المستند كاملًا) تعمل فقط إن فعّلت «السماح بقراءة نص الملفات». بدونه يرى
  المساعد الأسماء والبيانات ومقتطفات نتائج البحث فقط. الملفات الثنائية مرفوضة، وللحجم حد أقصى.
- **لا شبكة.** `athrix-mcp` لا يتصل بالإنترنت؛ يتحدث مع المساعد عبر الدخل والخرج القياسيين (stdio) فقط. **لكن انتبه:**
  ما يقرؤه المساعد قد يُرسَل إلى خدمته السحابية حسب المساعد الذي تستخدمه.
- **سجلّ.** كل استدعاء يُسجَّل في `%APPDATA%\com.samt.athrix\logs\mcp.log`: الأداة، والاستعلام أو المسار، وعدد النتائج،
  والزمن. **لا يُسجَّل محتوى الملفات.** الملف يُدوَّر عند 512 ك.ب (يبقى `mcp.log.1` واحد).

## التفعيل

1. افتح أثر ← الإعدادات (`Ctrl+,`) ← **المساعدات الذكية**.
2. فعّل «تفعيل خادم MCP»، ثم «أضف مجلدًا…» واختر المجلدات التي تسمح بها.
3. (اختياري) فعّل «السماح بقراءة نص الملفات».
4. اضغط «نسخ إعداد Claude Desktop» وألصقه في إعداد المساعد (أدناه).

التغييرات تسري فورًا: الخادم يقرأ `mcp.json` من جديد عند كل استدعاء، فلا حاجة لإعادة تشغيل المساعد. ولا يلزم أن يكون أثر
مفتوحًا: يكفي أنه فهرس الملفات مرة.

> **تنبيه:** يبحث الخادم في **لقطة** فهرس الأسماء التي يحفظها أثر، وقد تتأخر بضع دقائق عن القرص. الملفات المحذوفة منذ
> ذلك تُستبعد تلقائيًا، أما الجديدة جدًا فقد لا تظهر بعد. البحث في المحتوى يحتاج تفعيل «فهرسة المحتوى» في أثر.

## الإعداد في المساعد

### Claude Desktop
الإعدادات ← Developer ← Edit Config، ثم أضف إلى `claude_desktop_config.json` (الزر في أثر ينسخ هذا بالمسار الصحيح):

```json
{
  "mcpServers": {
    "athrix": {
      "command": "C:\\Users\\<you>\\AppData\\Local\\Athrix\\athrix-mcp.exe"
    }
  }
}
```

ثم أعد تشغيل Claude Desktop.

### Claude Code

```powershell
claude mcp add athrix -- "C:\Users\<you>\AppData\Local\Athrix\athrix-mcp.exe"
```

(أضف `--scope user` ليعمل في كل المشاريع.)

### خيارات سطر الأوامر

```text
athrix-mcp [--data-dir <dir>] [--log-file <file> | --no-log] [--idle-release <seconds>]
```

`--data-dir` مجلد بيانات أثر (الافتراضي `%APPDATA%\com.samt.athrix`)، مفيد للاختبار.
`--idle-release` يحرّر فهرس الأسماء من الذاكرة بعد هذا العدد من الثواني بلا بحث (الافتراضي 300 أي خمس دقائق؛ `0` يبقيه
محمّلًا). البحث التالي يعيد تحميله في أقل من ثانية عادةً.

## الأدوات

| الأداة | المدخلات | ما تعيده |
|---|---|---|
| `search_files` | `query`، `limit?` | ملفات ومجلدات بالاسم والبيانات: `path` و`name` و`kind` و`size` و`modified` (UTC) |
| `search_content` | `query`، `limit?` | ملفات يطابق نصها الاستعلام، مع `snippet` (المطابقات بين `**…**`) و`matched` |
| `file_info` | `path` | النوع والحجم وتاريخا الإنشاء والتعديل والامتداد، وهل تستطيع `read_text` قراءته |
| `read_text` | `path`، `maxChars?` (افتراضي 20 000، أقصى 200 000) | نص المستند (txt والكود وWord وExcel وPowerPoint وOpenDocument وEPUB وHTML وEML وPDF) — فقط مع «السماح بقراءة نص الملفات» |

الاستعلامات بلغة أثر نفسها: كلمات عربية أو إنجليزية، `"عبارة"`، `-كلمة`، `OR`، `*.pdf`، والمرشحات `ext:` و`type:`
و`size:` و`dm:` و`path:` و`infolder:` و`content:` و`fuzzy:` وأسماؤها العربية (`نوع:` و`حجم:` و`تاريخ:`…).
`limit` محدود بالقيمة `maxResults` في `mcp.json` (50 افتراضيًا، 200 حدًّا).

ملفات PDF تُقرأ عبر عامل الاستخراج المعزول `athrix-extract-worker` الموجود بجوار `athrix-mcp`، لا داخل الخادم نفسه.

## تفاصيل تقنية

- البروتوكول: JSON-RPC 2.0 عبر stdio، رسالة في كل سطر. الإصدارات: `2025-11-25` و`2025-06-18` و`2025-03-26`
  و`2024-11-05` (يُفاوَض عليها في `initialize`). المدعوم: `initialize` و`ping` و`tools/list` و`tools/call`
  والإشعارات `notifications/initialized` و`notifications/cancelled`.
- البيانات: لقطة فهرس الأسماء `index\names.snap`، تُقرأ عند أول بحث وتُعاد عند تغيّرها أو تغيّر المجلدات المسموح بها.
  تُقرأ متدفقة ولا يُحتفظ إلا بالمجلدات المسموح بها وما تحتها (والمجلدات المؤدية إليها)، فالذاكرة بحجم الجزء المسموح لا
  بحجم اللقطة كلها؛ وتُحرَّر بعد خمس دقائق بلا بحث (`--idle-release`). مثال على لقطة اصطناعية من 6 ملايين مدخل (632 م.ب)
  مع 5% منها مسموحة: الذروة 60 م.ب بدل 619، وبعد الخمول 2 م.ب بدل 612. إن سُمح بكل جذور اللقطة تُحمَّل كاملة كما كانت
  (ثم تُحرَّر عند الخمول).
  وفهرس المحتوى `content\` (Tantivy) يُفتح للقراءة فقط عند كل استدعاء دون قفل الكاتب، فيبقى أثر يكتب فيه.
- الإعداد: `%APPDATA%\com.samt.athrix\mcp.json`:
  `{ "enabled": false, "allowedFolders": [], "allowContentRead": false, "maxResults": 50 }`.
- الشيفرة: `crates/mcp` (الخادم) و`apps/desktop/src-tauri/src/mcp.rs` (الإعدادات في التطبيق).

---

## English

**Athrix MCP server** — `athrix-mcp` lets MCP clients (Claude Desktop, Claude Code, …) search your files through
Athrix's indexes.

- **Off by default.** Every tool call is refused (with a message saying where to enable it) until you turn it on in
  Athrix → Settings → **AI assistants** and add at least one folder.
- **Allowed folders only.** Every result and every read is checked: normalized, case-insensitive on Windows, `..`
  resolved, UNC / device / `\\?\` paths and alternate data streams refused, then the real path (symlinks, junctions)
  resolved and checked again.
- **Read-only.** Four tools — `search_files`, `search_content`, `file_info`, `read_text` — and nothing that writes,
  deletes, moves, opens or runs files. `read_text` works only with "Allow reading file text" on; binary files are
  refused and sizes are capped.
- **No network.** stdio only. What the assistant reads may still reach its cloud service.
- **Logged** to `%APPDATA%\com.samt.athrix\logs\mcp.log` (tool, query or path, count, time; never file contents).
- Results come from Athrix's saved name-index snapshot, which can lag the disk by a few minutes (deleted files are
  dropped). Content search needs content indexing on. Athrix itself need not be running.

**Claude Desktop** (`claude_desktop_config.json`; the "Copy Claude Desktop config" button fills in the path):

```json
{ "mcpServers": { "athrix": { "command": "C:\\Users\\<you>\\AppData\\Local\\Athrix\\athrix-mcp.exe" } } }
```

**Claude Code:** `claude mcp add athrix -- "C:\Users\<you>\AppData\Local\Athrix\athrix-mcp.exe"`

Protocol: JSON-RPC 2.0 over stdio (versions 2025-11-25, 2025-06-18, 2025-03-26, 2024-11-05): `initialize`, `ping`,
`tools/list`, `tools/call`. Settings file: `<app_data>/mcp.json`.

Memory: the name snapshot is streamed and only the allowed folders, everything under them and the folders leading to
them are kept, so memory follows the allowed part rather than the whole snapshot; it is freed after 5 minutes without a
search (`--idle-release <seconds>`, `0` keeps it). On a synthetic 6-million-entry snapshot (632 MB) with 5 % allowed:
peak 60 MB instead of 619 MB, 2 MB when idle instead of 612 MB. When every root of the snapshot is allowed it is loaded
whole, as before, and still freed when idle.
