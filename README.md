# Athrix · أثر

بحث فوري في ملفات جهازك: بالاسم، وبالمحتوى، وبالمعنى. محلي بالكامل، بدعم عربي أصيل.

> قيد التطوير — المرحلة M0. الحالة التفصيلية في [`docs/03-PROGRESS-TRACKER.md`](docs/03-PROGRESS-TRACKER.md).

## البنية
| المسار | الدور |
|---|---|
| `crates/core` | الأنواع المشتركة، الأخطاء، السجلات |
| `crates/text-ar` | الطبقة العربية فوق [nass](https://github.com/samt-qa/nass) |
| `crates/fs-index` | فهرس الأسماء (مشي متوازٍ، MFT/USN) |
| `crates/query` | لغة الاستعلام |
| `crates/extract` · `content-index` | استخراج النص وفهرسة المحتوى (Tantivy) |
| `crates/semantic` · `ranking` · `store` · `daemon` | البحث الدلالي، الترتيب، التخزين، الخدمة الخلفية |
| `crates/cli` | الأمر `athrix` |
| `apps/desktop` | تطبيق سطح المكتب (Tauri 2 + SolidJS) |
| `docs/` | البحث، نظام التصميم، متتبّع التقدّم |

## التطوير
```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p athrix-cli -- normalize "مُحَاضَرَةُ الفِقْهِ"

# تطبيق سطح المكتب
cd apps/desktop
npm install
npm run tauri dev
```


## ☕ ادعم المشروع / Support the project

إذا أفادك هذا المشروع، يمكنك دعم تطويره عبر PayPal.
If this project helps you, you can support its development via PayPal.

[![PayPal](https://img.shields.io/badge/PayPal-Support-00457C?logo=paypal&logoColor=white)](https://paypal.me/SamtSoftware)

👉 https://paypal.me/SamtSoftware
