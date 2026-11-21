# المهمة: إضافة تنفيذ Rust رابع لمكتبة nass

أنت تعمل في مستودع **nass** (مواصفة التطبيع العربي والبحث غير الحسّاس للتشكيل، ولها حاليًا ثلاث حزم: Dart وTypeScript وPHP). المطلوب إضافة **حزمة Rust** في `packages/rust` (crate باسم `nass`) تنفّذ **SPEC v0.3** بالحرف وتجتاز **كل** متجهات الاختبار المشتركة، مثل الحزم الثلاث تمامًا.

المستهلك الأول لهذه الحزمة تطبيق سطح مكتب بـ Rust لبحث الملفات اسمه **Athrix**، وسيستخدمها في فهرسة Tantivy (Tokenizer) وفي إبراز المطابقات على النص الأصلي.

## 1. اقرأ قبل أن تكتب سطرًا
1. `spec/SPEC.md` كاملًا، وخصوصًا: §1 ترتيب الخطوات، §2–4 المستويات، §5 `normalizeWithMap`، §6 البحث، §9 صيغة ملفات الاختبار، §10–14 القرارات والتعديلات.
2. `tools/tables.src.json` و`tools/gen-tables.mjs`، وافهم كيف تُولَّد الجداول لكل لغة وكيف يعمل `--check`.
3. `packages/js` بوصفه التنفيذ المرجعي للبنية والـ API، ثم انظر في `packages/dart` و`packages/php` لترى كيف عولجت الفروق بين اللغات.
4. `spec/vectors/*.json` و`tools/check-vectors.mjs` و`tools/differential.mjs` و`.github/workflows/ci.yml`.
5. قواعد المساهمة في README: **أي تغيير في السلوك يبدأ في `SPEC.md` و`spec/vectors/`**، ورسائل commit بالإنجليزية، والتوثيق باللغتين.

## 2. قواعد صارمة
- **لا تغيّر السلوك.** حزمة Rust تتبع المواصفة، والمواصفة لا تتبعها. إن وجدت غموضًا في المواصفة أو تعارضًا بين المواصفة وحزمة أخرى، **توقّف واكتب تقريرًا** بالحالة والمثال، ولا تخترع قاعدة.
- **لا جداول مكتوبة باليد.** وسّع `tools/gen-tables.mjs` ليولّد `packages/rust/src/tables.rs` من `tables.src.json`، واجعل `--check` يغطّيه.
- **بلا اعتماديات وقت التشغيل** (مثل حزمة JS). إن احتاجت الخطوة C1 (التركيب القانوني في النطاق العربي) بيانات يونيكود، فولّد الجزء اللازم في الجداول ولا تضف `unicode-normalization`. الاعتماديات مسموحة في `[dev-dependencies]` فقط (`serde`/`serde_json` لقراءة المتجهات، `proptest`، `criterion`).
- `#![forbid(unsafe_code)]`، وRust stable، وحدّد MSRV في `Cargo.toml` (`rust-version`).
- لا تنشر على crates.io ولا تدفع (push) إلى `main` دون إذن صريح من المالك. اعمل على فرع.

## 3. الـ API المطلوب (بأسلوب Rust، ومكافئ لـ JS)
```rust
pub enum Level { Clean, Search /* default */, Loose }
pub enum Honorifics { Keep /* default */, Strip }
pub struct Options { pub level: Level, pub honorifics: Honorifics /* + أي خيار آخر في SPEC */ }

pub fn normalize(text: &str, opts: &Options) -> String;
pub fn normalize_into(text: &str, opts: &Options, out: &mut String); // يعيد استخدام المخزن: مهم للفهرسة
pub fn normalize_with_map(text: &str, opts: &Options) -> Normalized;  // النص + خريطة إلى مواضع الأصل
pub fn find_all(text: &str, query: &str, opts: &SearchOptions) -> Vec<Match>; // matchMode، wholeWord...
pub fn highlight(text: &str, query: &str, opts: &HighlightOptions) -> String;
pub fn name_key(name: &str) -> String;

#[cfg(feature = "isnad")]   // تجريبي، خلف feature منفصل كما في JS (`/isnad`)
pub mod isnad { pub fn split_isnad(text: &str) -> IsnadResult; }
```
- يجب أن تكون كل الأنواع `Send + Sync`، وأن تشتق `Debug, Clone, PartialEq` حيث يناسب، و`Default` للخيارات.
- `Normalized` يجب أن يسمح بتحويل أي مدى في النص المُطبَّع إلى مداه في النص الأصلي (هذا ما يحتاجه Tokenizer في Athrix).

## 4. ⚠️ وحدة المواضع (offsets)
- Rust يعمل بمواضع **البايت في UTF-8**، وTS بوحدات **UTF-16**، وPHP **بنقاط الترميز** (code points).
- الـ API الأصلي في Rust يعيد **مواضع بايت** (لأن `&text[start..end]` يعمل بها مباشرة، وTantivy يتوقعها).
- أضف دوال تحويل عامة: `byte_to_char_offset` و`byte_to_utf16_offset` (أو طرقًا على `Match`)، كي يطابق المستهلك ما تعيده حزمة TS في الواجهة.
- حدّد الوحدة التي تستخدمها المتجهات (راجع SPEC §5 و§9)، وحوّل في **حزام الاختبار** لا في المكتبة. وثّق هذا في README الحزمة، واقترح (دون أن تعدّل بنفسك) سطرًا في SPEC يذكر وحدة المواضع في Rust.

## 5. الاختبارات (معيار القبول)
1. اختبار تكامل يقرأ `spec/vectors/normalize.json` و`offsets.json` و`search.json` و`names.json` (و`isnad.json` مع feature `isnad`) ويمرّ **100%**، ويطبع عند الفشل رقم الحالة والمتوقع والناتج.
2. اختبارات خصائص بـ `proptest`: الـ idempotence لكل مستوى، و`clean ⊂ search`، وصحة الخريطة (كل مدى مُطبَّع يقع داخل حدود محارف صالحة في الأصل).
3. أضف Rust إلى `tools/differential.mjs` (أو أداة مكافئة) لتُقارَن مع الأوراكل المستقل على نصوص الكتب الستة: الهدف **0 اختلافات**.
4. قياس أداء بـ `criterion` على 1MB من صحيح البخاري المشكول (نفس نص القياس في README)، لكل مستوى. الهدف: أسرع من Dart (24/26/28ms).

## 6. CI
أضف job في `ci.yml`: `cargo fmt --check`، و`cargo clippy --all-targets --all-features -- -D warnings`، و`cargo test --all-features`، على stable وعلى MSRV، وعلى Ubuntu وWindows. وأضف فحص `gen-tables --check` للجدول المولَّد.

## 7. التوثيق
- `packages/rust/README.md`: التثبيت، أمثلة مكافئة لـ TS، شرح وحدة المواضع، والـ features.
- حدّث `README.md` و`README.ar.md`: جدول الحزم، وجدول الجودة (التغطية)، وجدول الأداء، وصف جدول المقارنة («Same behaviour across languages»).
- `CHANGELOG.md`.
- جهّز بيانات `Cargo.toml` للنشر (license MIT، repository، keywords، categories، readme)، وتحقّق إن كان اسم `nass` متاحًا على crates.io (وإلا اقترح بديلًا مثل `nass-ar`). **لا تنشر.**

## 8. التقرير النهائي
في نهاية العمل اكتب تقريرًا قصيرًا يتضمن:
- نتيجة كل ملف متجهات (عدد الحالات الناجحة من الإجمالي)، ونتيجة الاختبار التفاضلي.
- أرقام الأداء للمستويات الثلاثة.
- أي غموض وجدته في المواصفة أو اختلاف بين الحزم (مع أمثلة)، وما قرّرته بشأنه.
- ما لم يُنجَز أو لم يُتحقَّق منه، بوضوح.
- اسم الفرع، وهل الحزمة جاهزة للنشر.
