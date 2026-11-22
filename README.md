<div align="center">

# Athrix · أثر

**بحث فوري في ملفات جهازك: بالاسم وبالمحتوى، محلي بالكامل، بدعم عربي أصيل.**

**Instant search across your files, by name and by content. Fully local, Arabic-first.**

[![CI](https://github.com/samt-qa/Athrix/actions/workflows/ci.yml/badge.svg)](https://github.com/samt-qa/Athrix/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#الترخيص--license)
[![Rust 1.85+](https://img.shields.io/badge/rust-1.85%2B-orange.svg?logo=rust)](https://www.rust-lang.org)
[![Platform: Windows](https://img.shields.io/badge/platform-Windows-0078D6.svg?logo=windows)](#التثبيت)
[![Donate](https://img.shields.io/badge/Donate-PayPal-00457C.svg?logo=paypal)](https://paypal.me/SamtSoftware)

[العربية](#نظرة-عامة) · [English](#english)

</div>

> [!NOTE]
> Athrix قيد التطوير النشط ولم يصدر منه إصدار مستقر بعد. الحالة التفصيلية في [`docs/03-PROGRESS-TRACKER.md`](docs/03-PROGRESS-TRACKER.md)، وما تبقّى في [`docs/05-REMAINING-WORK.md`](docs/05-REMAINING-WORK.md).

## نظرة عامة

أثر برنامج سطح مكتب يجد أي ملف على جهازك لحظة الكتابة، ويبحث داخل المستندات أيضًا. صُمّم للعربية من الأساس: يفهم التشكيل والهمزات والتاء المربوطة، ويصحّح الكتابة بلوحة المفاتيح الخطأ، ويعرض الواجهة من اليمين إلى اليسار. كل شيء يعمل على جهازك، فلا يخرج اسم ملف ولا محتواه إلى الإنترنت.

## المزايا

### البحث بالاسم
- **نتائج أثناء الكتابة** على ملايين الملفات: أول نتيجة في نحو 12 ملي ثانية على 6.9 مليون ملف.
- **الجهاز كاملًا أو قرص بعينه**، مع الأقراص الخارجية: تُفهرس الفلاشة عند توصيلها وتختفي نتائجها عند فصلها.
- **متابعة حيّة** للتغييرات: الملفات الجديدة والمحذوفة تظهر وتختفي فورًا.
- **بحث تقريبي** (`fuzzy:`) يجد «تقرير المبيعات» من «تقرر مبيع»، والاختصارات مثل `rpt`.

### البحث في المحتوى
- **داخل المستندات:** TXT وملفات البرمجة، وWord وExcel وPowerPoint، وOpenDocument، وEPUB، وHTML، والبريد EML، وPDF.
- **مقاطع مُبرزة** تحت كل نتيجة تُظهر موضع الكلمة، على النص الأصلي بتشكيله.
- **فهرسة في الخلفية** تتوقف تلقائيًا عند العمل على البطارية أو انشغال الجهاز، مع إيقاف واستئناف يدوي.
- **عزل ملفات PDF** في عملية منفصلة، فلا يُسقط ملف تالف البرنامج.

### العربية أولًا
- تطبيع ذكي عبر [nass](https://github.com/samt-qa/nass): التشكيل، والتطويل، وأشكال الألف، والياء والألف المقصورة، والأرقام العربية والفارسية.
- **تصحيح لوحة المفاتيح:** كتبت `hgjrvdv` بدل «التقرير»؟ يقترح أثر الصحيح.
- مرشّحات بالعربية: `امتداد:pdf`، `حجم:>1mb`، `محتوى:"عقد الإيجار"`، `مجلد:"المستندات"`.
- واجهة RTL كاملة، والإنجليزية متاحة.

### تجربة الاستخدام
- **نافذة بحث سريع** من أي برنامج: `Ctrl+Shift+Space`.
- **معاينة** للنصوص والكود والمستندات والصور وPDF، مع التنقّل بين المطابقات.
- **المكتبة:** السجلّ، والبحوث المحفوظة، والمفضّلة.
- **قائمة الزر الأيمن في المستكشف:** «ابحث هنا بـ Athrix».
- سهولة الوصول: قارئ الشاشة، وتركيز ظاهر، وتباين مدروس، والوضع الداكن، والتباين العالي.

## لغة الاستعلام

| تكتب | يجد |
|---|---|
| `تقرير 2024` | ملفات اسمها يحوي الكلمتين |
| `ext:pdf` أو `امتداد:pdf` | ملفات PDF فقط |
| `type:image` أو `نوع:صورة` | الصور |
| `size:>10mb` | الأكبر من 10 ميجابايت |
| `dm:thisweek` | المعدّلة هذا الأسبوع |
| `content:"عقد الإيجار"` | المستندات التي تحوي العبارة |
| `فاتورة content:وزارة` | اسمها «فاتورة» ومحتواها «وزارة» |
| `infolder:"D:\Work"` | داخل مجلد بعينه |
| `fuzzy:rpt` | بحث تقريبي واختصارات |
| `report \| تقرير` و `!tmp` | «أو» والاستثناء |

## الاختصارات

| الاختصار | الوظيفة |
|---|---|
| `Ctrl+Shift+Space` | نافذة البحث السريع من أي مكان |
| `↑` `↓` · `Enter` | التنقّل بين النتائج · فتح النتيجة |
| `Ctrl+Enter` | فتح المجلد الذي يحويها |
| `Ctrl+C` | نسخ المسار |
| `←` (بالعربية) · `→` (بالإنجليزية) | فتح المعاينة |
| `F3` · `Shift+F3` | المطابقة التالية والسابقة في المعاينة |
| `Ctrl+D` · `Ctrl+S` | إضافة للمفضّلة · حفظ البحث |
| `Shift+F10` | قائمة أوامر النتيجة |
| `Ctrl+,` · `Esc` | الإعدادات · مسح أو إغلاق |

## الخصوصية

- **لا اتصال بالإنترنت افتراضيًا.** فحص التحديثات يدوي ما لم تفعّله بنفسك.
- **لا تتبّع ولا إحصاءات.** الفهارس تبقى في مجلد بيانات التطبيق على جهازك.
- **لا يحتاج صلاحيات مسؤول.** التثبيت لكل مستخدم.

## التثبيت

لا يوجد إصدار جاهز للتنزيل بعد. عند صدوره سيكون في صفحة [Releases](https://github.com/samt-qa/Athrix/releases). حتى ذلك الحين يمكنك البناء من المصدر.

**المتطلبات:** ويندوز 10 أو 11، و[Rust](https://rustup.rs) 1.85 أو أحدث، و[Node.js](https://nodejs.org) 22، و[متطلبات Tauri 2](https://v2.tauri.app/start/prerequisites/).

```sh
git clone https://github.com/samt-qa/Athrix.git
cd Athrix/apps/desktop
npm install
npm run tauri dev      # تشغيل للتطوير
npm run tauri build    # مثبّت NSIS في target/release/bundle/nsis
```

## البنية

| المسار | الدور |
|---|---|
| `crates/text-ar` | الطبقة العربية: التطبيع، والتجذيع الخفيف، والتقطيع مع خريطة المواضع |
| `crates/fs-index` | فهرس الأسماء: المشي المتوازي، والمتابعة الحيّة، واللقطات، والبحث التقريبي |
| `crates/ntfs` | قراءة MFT ومتابعة USN على NTFS |
| `crates/query` | لغة الاستعلام وتخطيط التنفيذ |
| `crates/extract` | استخراج النص من المستندات، وعامل PDF المعزول |
| `crates/content-index` | فهرس المحتوى (Tantivy) بمحلّل عربي ومقاطع |
| `crates/ranking` | دمج نتائج الاسم والمحتوى (RRF) والتكرار والحداثة |
| `crates/daemon` | جدولة فهرسة المحتوى في الخلفية |
| `crates/store` | السجلّ والبحوث المحفوظة والمفضّلة (SQLite) |
| `crates/cli` | أداة سطر الأوامر `athrix` (فهرسة، وبحث، وقياس أداء) |
| `apps/desktop` | تطبيق سطح المكتب: Tauri 2 + SolidJS |

**التوثيق:** [البحث والمزايا](docs/01-RESEARCH-AND-FEATURES.md) · [نظام التصميم](docs/02-DESIGN-SYSTEM.md) · [عقد الواجهة](docs/IPC-CONTRACT.md) · [تدقيق الأداء](docs/PERF-AUDIT.md) · [الإصدار](docs/RELEASE.md)

## المساهمة

المساهمات مرحّب بها. اقرأ [`CONTRIBUTING.md`](CONTRIBUTING.md) قبل البدء، والتزم بـ [ميثاق السلوك](CODE_OF_CONDUCT.md). للإبلاغ عن ثغرة أمنية راجع [`SECURITY.md`](SECURITY.md) ولا تفتح Issue عامًا.

## ☕ ادعم المشروع / Support the project

أثر مجاني ومفتوح المصدر. إذا أفادك هذا المشروع، يمكنك دعم تطويره عبر PayPal.
If this project helps you, you can support its development via PayPal.

[![PayPal](https://img.shields.io/badge/PayPal-Support-00457C?logo=paypal&logoColor=white&style=for-the-badge)](https://paypal.me/SamtSoftware)

👉 https://paypal.me/SamtSoftware

## الترخيص / License

مرخّص بأحد الترخيصين التاليين، على اختيارك:

- Apache License 2.0 ([`LICENSE-APACHE`](LICENSE-APACHE))
- MIT License ([`LICENSE-MIT`](LICENSE-MIT))

ما لم تصرّح بغير ذلك، فأي مساهمة تقدّمها عمدًا لإدراجها في هذا المشروع، كما يعرّفها ترخيص Apache-2.0، تُرخَّص بالترخيصين معًا كما أعلاه، بلا أي شروط إضافية.

---

<a id="english"></a>

## English

Athrix is a desktop app that finds any file on your computer as you type, and searches inside documents too. It is built for Arabic from the ground up: it understands diacritics, hamza and alef forms, and taa marbuta; it fixes text typed with the wrong keyboard layout; and the interface is right-to-left first, with English also available. Everything runs on your machine, so no file name or content ever leaves it.

**Highlights**
- **Name search as you type** across millions of files: about 12 ms for the first result on 6.9M entries.
- **Live updates**, plus removable drives indexed when plugged in.
- **Content search** in TXT and source code, Word, Excel, PowerPoint, OpenDocument, EPUB, HTML, EML and PDF.
  - Highlighted snippets on the original diacritized text.
  - Background indexing that pauses on battery or when the machine is busy.
  - PDFs are parsed in a separate process, so a damaged file can't crash the app.
- **A query language** with English and Arabic filters: `ext:`, `type:`, `size:`, `dm:`, `content:`, `infolder:`, `fuzzy:`, OR and NOT.
- **Quick search popup** from anywhere (`Ctrl+Shift+Space`), a preview pane, history, saved searches, favorites, and "Search here with Athrix" in Explorer.
- **Private by design:** no network access by default, no telemetry, no admin rights needed.

**Status:** under active development; no stable release yet. Windows first. See the [progress tracker](docs/03-PROGRESS-TRACKER.md).

**Build from source:** Windows 10 or 11, Rust 1.85+, Node.js 22 and the [Tauri 2 prerequisites](https://v2.tauri.app/start/prerequisites/), then:

```sh
cd apps/desktop
npm install
npm run tauri dev
```

**Contributing:** see [`CONTRIBUTING.md`](CONTRIBUTING.md), the [Code of Conduct](CODE_OF_CONDUCT.md) and the [security policy](SECURITY.md).

**Support the project:** [paypal.me/SamtSoftware](https://paypal.me/SamtSoftware)

**License:** dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option. Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in Athrix, as defined in the Apache-2.0 license, shall be dual-licensed as above, without any additional terms or conditions.
