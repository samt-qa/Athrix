# المساهمة في Athrix

شكرًا لاهتمامك. هذا الملف يشرح كيف تجهّز بيئة العمل، وما المطلوب قبل أي commit.
الحالة التفصيلية والمهمة التالية في [`docs/03-PROGRESS-TRACKER.md`](docs/03-PROGRESS-TRACKER.md): ابدأ من القسم 4.

## المتطلبات
| الأداة | الإصدار |
|---|---|
| Rust | stable، والحد الأدنى المدعوم (MSRV) **1.85** — لا تستخدم واجهات أحدث منه |
| Node.js | 22 |
| Tauri | متطلبات النظام من [دليل Tauri 2](https://v2.tauri.app/start/prerequisites/) (WebView2 على ويندوز، webkit2gtk-4.1 على لينكس) |

للتحقق من MSRV محليًا: `rustup toolchain install 1.85`.

## التشغيل
```sh
cargo build --workspace
cargo run -p athrix-cli -- normalize "مُحَاضَرَةُ الفِقْهِ"

cd apps/desktop
npm install
npm run tauri dev      # التطبيق كاملًا
npm run dev            # الواجهة وحدها في المتصفح مع خلفية وهمية (lib/mockBackend.ts)
```
السجلات: متغير البيئة `ATHRIX_LOG` (مثل `ATHRIX_LOG=debug`).

## قبل كل commit (تعريف "منجز")
هذه الأوامر نفسها يشغّلها CI على ويندوز ولينكس، على stable و1.85:
```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo +1.85 clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cd apps/desktop && npm run typecheck && npm run build
```
- كل سلوك جديد معه اختبار. الأداء الحساس معه bench (`cargo bench -p athrix-fs-index`).
- لا `unsafe` (الـ workspace يمنعه)، وكل عنصر عام موثّق (`missing_docs`).
- حدّث المتتبّع: حالة المهمة، سجل الجلسة، و"الخطوة التالية".

## الأعراف
- **الرسائل:** [Conventional Commits](https://www.conventionalcommits.org/) أو جملة إنجليزية واضحة بصيغة الأمر تصف السبب لا الملف.
- **نصوص الواجهة:** لا نصوص ثابتة في المكوّنات؛ كل نص في `apps/desktop/src/i18n/{ar,en}.json` عبر `t()`. العربية أولًا، والتخطيط يدعم RTL.
- **التصميم:** المكوّنات تستخدم الرموز الدلالية في `styles/tokens.css` فقط، لا ألوان HEX مباشرة. المرجع [`docs/02-DESIGN-SYSTEM.md`](docs/02-DESIGN-SYSTEM.md).
- **الربط بين الواجهة وRust:** أي تغيير في الأوامر أو الأنواع يُكتب أولًا في [`docs/IPC-CONTRACT.md`](docs/IPC-CONTRACT.md). الواجهة لا ترسل مسارات ملفات أبدًا، بل أرقام العناصر.
- **العربية:** التطبيع يمر عبر `crates/text-ar` (فوق [nass](https://github.com/samt-qa/nass)) لا عبر كود خاص.
- **الشبكة:** لا اتصال بالشبكة بلا قرار معماري (ADR) في المتتبّع. Athrix محلي بالكامل.
- **الإصدارات:** حزم npm لـ Tauri مثبّتة لتطابق crates Rust المقيّدة بـ MSRV؛ إن رفعت إحداها فارفع الأخرى معها.

## القرارات المعمارية
القرارات الكبيرة تُسجَّل كـ ADR في القسم 6 من المتتبّع قبل التنفيذ: السياق، الخيارات، القرار، السبب.

## الإبلاغ عن الأخطاء والاقتراحات
- استخدم قوالب الـ Issues (بلاغ خطأ / اقتراح ميزة)، بالعربية أو الإنجليزية.
- الثغرات الأمنية تُبلَّغ بشكل خاص؛ راجع [`SECURITY.md`](SECURITY.md).
- بالمشاركة توافق على [ميثاق السلوك](CODE_OF_CONDUCT.md).

## الترخيص
المشروع مرخّص بـ **MIT OR Apache-2.0** (راجع [`LICENSE-MIT`](LICENSE-MIT) و[`LICENSE-APACHE`](LICENSE-APACHE)). ما لم تصرّح بغير ذلك، فأي مساهمة تقدّمها عمدًا لإدراجها في المشروع، كما يعرّفها ترخيص Apache-2.0، تُرخَّص بالترخيصين معًا بلا أي شروط إضافية.
