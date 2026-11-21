# عقد الربط بين الواجهة و Rust (IPC Contract) — v1

> هذا العقد يتفق عليه طرفان يعملان بالتوازي: الواجهة (`apps/desktop/src`) والخلفية (`apps/desktop/src-tauri`).
> أي تغيير فيه يُسجَّل هنا أولًا. أسماء الحقول في JSON بصيغة camelCase (serde `rename_all = "camelCase"`).

## الأنواع

```ts
type IndexState = "idle" | "indexing" | "ready" | "error";

interface IndexStatus {
  state: IndexState;
  entries: number;          // عدد العناصر المفهرسة (0 أثناء أول فهرسة)
  roots: string[];          // المجلدات المفهرسة
  elapsedMs: number | null; // مدة آخر فهرسة مكتملة
  error: string | null;     // رسالة إنجليزية عند state = "error"
}

interface ResultItem {
  id: number;               // EntryId — يُستخدم في أوامر الفتح، لا يُرسل مسار من الواجهة أبدًا
  name: string;             // الاسم الأصلي كما على القرص
  parent: string;           // المسار الكامل للمجلد الأب
  isDir: boolean;
  size: number | null;      // بالبايت؛ null للمجلدات
  modified: number | null;  // Unix seconds
  ext: string;              // الامتداد بحروف صغيرة بلا نقطة، "" إن لم يوجد
  highlights: [number, number][]; // مدى المطابقات داخل name بوحدات UTF-16 (مناسبة لـ JS string.slice)
}

interface QueryError {
  message: string;          // إنجليزية
  kind: string;             // رمز ثابت من ParseErrorKind (مثل "UnknownFilter") لترجمته في الواجهة،
                            // أو "Unsupported": مرشّح صحيح لا يدعمه فهرس الأسماء بعد (content:, sem:, ~, tag:...)
  start: number;            // مدى الخطأ في نص الاستعلام بوحدات UTF-16
  end: number;
  suggestion: string | null;
}

interface SearchResponse {
  requestId: number;        // يُعاد كما أُرسل؛ الواجهة تتجاهل أي استجابة أقدم من آخر طلب
  items: ResultItem[];
  truncated: boolean;       // true إن وُجدت نتائج أكثر من limit
  elapsedMs: number;
  error: QueryError | null; // عند خطأ صياغة: items فارغة
  cancelled: boolean;       // true إن ألغاه طلب أحدث
}
```

## الأوامر (`invoke`)

| الأمر | المدخلات | المخرجات | ملاحظات |
|---|---|---|---|
| `index_status` | — | `IndexStatus` | |
| `start_indexing` | `{ roots?: string[] }` | `void` | يبدأ الفهرسة في الخلفية ويعود فورًا. بلا `roots`: مجلد المستخدم (home). يُستدعى تلقائيًا عند الإقلاع |
| `search` | `{ query: string, limit: number, requestId: number }` | `SearchResponse` | كل طلب جديد يلغي السابق. الاستعلام الفارغ ⇠ `items: []`. إن لم يكن الفهرس جاهزًا ⇠ `items: []` بلا خطأ |
| `open_entry` | `{ id: number }` | `void` | يفتح الملف بالبرنامج الافتراضي |
| `reveal_entry` | `{ id: number }` | `void` | يفتح المجلد ويحدّد العنصر |
| `entry_path` | `{ id: number }` | `string` | المسار الكامل (للنسخ بـ `navigator.clipboard`) |
| `app_info` | — | `{ name, version }` | موجود |

الأخطاء غير المتوقعة في الأوامر تُرفض كـ string (رسالة إنجليزية).

## الأحداث (`listen`)

| الحدث | الحمولة | متى |
|---|---|---|
| `index://status` | `IndexStatus` | عند كل تغيّر في الحالة (بدء، اكتمال، خطأ) |

## الاستثناءات الافتراضية للفهرسة
`.git`، `node_modules`، `$RECYCLE.BIN`، `System Volume Information`، `AppData\Local\Temp`.
