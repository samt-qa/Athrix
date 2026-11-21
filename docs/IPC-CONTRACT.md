# عقد الربط بين الواجهة و Rust (IPC Contract) — v1

> هذا العقد يتفق عليه طرفان يعملان بالتوازي: الواجهة (`apps/desktop/src`) والخلفية (`apps/desktop/src-tauri`).
> أي تغيير فيه يُسجَّل هنا أولًا. أسماء الحقول في JSON بصيغة camelCase (serde `rename_all = "camelCase"`).

## الأنواع

```ts
type IndexState = "idle" | "indexing" | "ready" | "error";

interface IndexStatus {
  state: IndexState;
  entries: number;          // عدد عناصر الفهرس المستخدم في البحث (0 أثناء أول فهرسة إن لم توجد لقطة)
  roots: string[];          // المجلدات المفهرسة
  elapsedMs: number | null; // مدة آخر فهرسة مكتملة؛ null إن كان الفهرس الحالي محمّلًا من لقطة
  error: string | null;     // رسالة إنجليزية عند state = "error"
}

interface ResultItem {
  id: number;               // معرّف نتيجة = (جيل الفهرس << 32) | رقم العنصر، دائمًا < 2^53 (آمن في JS).
                            // يُستخدم في أوامر الفتح كما هو (رقم معتم، لا تحلّله الواجهة). لا يُرسل مسار من الواجهة أبدًا
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

type VolumeKind = "fixed" | "removable" | "network" | "unknown";

interface Volume {          // قرص في هذا الجهاز (list_volumes)
  path: string;             // جذر القرص: "C:\\" (ويندوز) أو "/" و "/media/usb" (غيره)
  name: string;             // اسم قصير للواجهة: "C:"؛ والمسار نفسه خارج ويندوز
  label: string | null;     // تسمية القرص ("Data") إن وُجدت
  kind: VolumeKind;         // fixed = داخلي (يُفهرس افتراضيًا)؛ removable/unknown لا يُفهرسان افتراضيًا
}

interface IndexRoots {      // إعداد "ماذا نفهرس" (get_index_roots / set_index_roots)
  wholeComputer: boolean;   // true = كل الجهاز (كل الأقراص الداخلية، يُعاد حسابها عند كل إقلاع)
  roots: string[];          // المجلدات التي يفهرسها هذا الإعداد الآن
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
| `start_indexing` | `{ roots?: string[] }` | `void` | يبدأ الفهرسة في الخلفية ويعود فورًا (طلب أثناء فهرسة جارية يُصفّ، الأحدث يفوز). بلا `roots`: المجلدات المحفوظة في الإعداد (انظر `set_index_roots`؛ الافتراضي كل الجهاز). `roots` الصريحة تُفهرس مرة ولا تُحفظ؛ مسار نسبي ⇠ رفض |
| `list_volumes` | — | `Volume[]` | أقراص الجهاز مرتبة بالمسار. ويندوز: كل قرص محلي له حرف (داخلي أو قابل للإزالة) مع تسميته؛ أقراص الشبكة والأقراص الضوئية لا تظهر. غيره: `/` والأقراص تحت `/media` و `/mnt` و `/Volumes` |
| `get_index_roots` | — | `IndexRoots` | الإعداد الحالي والمجلدات التي يفهرسها |
| `set_index_roots` | `{ roots: string[] \| null }` | `IndexRoots` | `null` = كل الجهاز؛ وإلا هذه الأقراص/المجلدات بالضبط (`"d:"` و `"D:/"` ⇠ `"D:\\"`، والمكرر أو المتداخل يُحذف). يُحفظ في `<app_data>/indexed-roots.json` ويُستخدم عند الإقلاع قبل تحميل الواجهة. إن تغيّرت المجلدات تبدأ إعادة فهرسة (تُصفّ خلف فهرسة جارية، الأحدث يفوز). قائمة فارغة أو مسار نسبي ⇠ رفض |
| `search` | `{ query: string, limit: number, requestId: number, scope?: string \| null }` | `SearchResponse` | كل طلب جديد يلغي السابق. الاستعلام الفارغ ⇠ `items: []`. إن لم يكن الفهرس جاهزًا ⇠ `items: []` بلا خطأ. `scope`: قرص (`"D:\\"`، يقبل `"d:"`) أو مجلد مطلق؛ تُعاد فقط العناصر تحته (مقارنة غير حساسة لحالة الأحرف على ويندوز). `null`/فارغ = كل الفهرس. نطاق خارج كل المجلدات المفهرسة ⇠ `items: []` بلا خطأ؛ مسار نسبي ⇠ رفض بـ `"invalid search scope: …"` |
| `open_entry` | `{ id: number }` | `void` | يفتح الملف بالبرنامج الافتراضي |
| `reveal_entry` | `{ id: number }` | `void` | يفتح المجلد ويحدّد العنصر |
| `entry_path` | `{ id: number }` | `string` | المسار الكامل (للنسخ بـ `navigator.clipboard`) |
| `app_info` | — | `{ name, version }` | موجود |

الأخطاء غير المتوقعة في الأوامر تُرفض كـ string (رسالة إنجليزية).

### أجيال الفهرس ومعرّفات النتائج
- كل فهرس يصبح حاليًا (نتيجة مشي أو لقطة محمّلة) يأخذ **جيلًا** جديدًا (21 بت، يلتف عند 2^21).
- `ResultItem.id` يحمل الجيل ورقم العنصر: `generation * 2^32 + entry`. الشكل في JSON ما زال `number`.
- `open_entry` / `reveal_entry` / `entry_path` بمعرّف من جيل أقدم (بعد إعادة الفهرسة) تُرفض بالرسالة الثابتة
  `"this result is out of date; search again"` — ولا تُحلّ أبدًا إلى ملف آخر. على الواجهة عرض رسالة "أعد البحث" (أو إعادة البحث تلقائيًا).
- معرّف لا يمكن أن يصدر عن الخلفية (≥ 2^53) يُرفض بـ `"invalid result id …"`؛ وقبل جاهزية أي فهرس: `"the index is not ready yet"`.

## الأحداث (`listen`)

| الحدث | الحمولة | متى |
|---|---|---|
| `index://status` | `IndexStatus` | عند كل تغيّر في الحالة (بدء، اكتمال، خطأ، تحميل لقطة) |

## لقطة الفهرس عند الإقلاع (المهمة 1.4)
- تُحفظ في `<app_data>/index/names.snap` (مجلد بيانات التطبيق لـ `com.samt.athrix`) مع ملف جانبي
  `names.roots.json` يذكر المجلدات التي تغطيها. تُحفظ بعد كل مشي ناجح (في خيط الفهرسة، بعد تبديل الفهرس).
- عند الإقلاع، إن وُجدت لقطة سليمة لنفس المجلدات بالضبط، تكون تسلسل الأحداث:
  `indexing (entries: 0)` ← `ready (entries: N, elapsedMs: null)` ← `indexing (entries: N)` ← `ready (entries: M, elapsedMs: …)`.
  البحث يعمل من حدث `ready` الأول؛ ومعرّفات نتائج اللقطة تصبح قديمة بعد `ready` الثاني.
- لقطة تالفة أو من إصدار آخر أو لمجلدات أخرى تُتجاهل (تُسجَّل في السجل فقط) ويجري مشي عادي: `indexing` ← `ready`.
- فشل حفظ اللقطة لا يظهر كخطأ فهرسة (يُسجَّل فقط).

## المجلدات المفهرسة (الإعداد الافتراضي: كل الجهاز)
- الافتراضي **كل الجهاز**: كل الأقراص الداخلية (`kind: "fixed"`) على ويندوز، مثل `["C:\\", "D:\\"]`؛ و `["/"]` خارج ويندوز.
  إن لم يوجد قرص داخلي ⇠ مجلد المستخدم. الأقراص القابلة للإزالة والشبكة لا تُفهرس إلا باختيارها.
- الإعداد يُحفظ في `<app_data>/indexed-roots.json` بالشكل `{"version":1,"roots":null}` (كل الجهاز) أو
  `{"version":1,"roots":["C:\\","D:\\"]}`. ملف مفقود أو تالف أو من إصدار آخر ⇠ كل الجهاز (يُسجَّل فقط).
- `IndexStatus.roots` يذكر دائمًا المجلدات الفعلية؛ الواجهة تعرضها في شريط الحالة وتبني منها محدّد نطاق البحث.
- لقطة الفهرس تُستخدم عند الإقلاع فقط إن طابقت مجلداتها المجلدات المحفوظة (انظر أعلاه)؛ تغيير الأقراص يعني مشيًا كاملًا.

## نطاق البحث (`search.scope`)
- الواجهة تعرض قبل الكتابة: «كل الجهاز» + قرص لكل مجلد في `IndexStatus.roots` (يُخفى المحدّد إن كان هناك مجلد واحد).
  الاختيار يُحفظ في التخزين المحلي (`athrix.searchScope`) ويُشارك بين النافذة الرئيسية والنافذة السريعة؛
  اختيار قرص لم يعد مفهرسًا يُعامل ككل الجهاز.
- التنفيذ في Rust: كل جذر مفهرس يشغل كتلة معرّفات متصلة (الجذر ثم شجرته كاملة)، فيُفحص فقط ما يقع في النطاق،
  بالتوازي وبالترتيب نفسه وبالتوقف المبكر عند `limit + 1`. لا يتغيّر نص الاستعلام.

## الاستثناءات الافتراضية للفهرسة
- بالاسم (أي مستوى، غير حساس لحالة الأحرف على ويندوز): `.git`، `node_modules`، `$RECYCLE.BIN`، `System Volume Information`،
  `$WinREAgent`، `$Windows.~BT`، `$Windows.~WS`، `$SysReset` (مجلدات تحديث/استعادة ويندوز المؤقتة).
- بالمسار: `**/AppData/Local/Temp`، `**/Windows/WinSxS` (مخزن مكونات ويندوز: نحو 100 ألف نسخة مرتبطة من ملفات النظام).
- خارج ويندوز فقط: `/proc`، `/sys`، `/dev`.
- لا تُتبع الروابط الرمزية ولا الـ junctions (مثل `Documents and Settings`). الملفات مثل `pagefile.sys` و `hiberfil.sys` تُفهرس كأسماء عادية.
