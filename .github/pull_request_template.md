## ماذا يغيّر هذا الطلب؟ · What does this change?

<!-- الوصف والسبب. Link the issue: Closes #… -->

## كيف تحقّقت؟ · How was it verified?

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --locked -- -D warnings` (stable و 1.85)
- [ ] `cargo test --workspace --locked`
- [ ] `cd apps/desktop && npm run typecheck && npm run build && npm run check:query`
- [ ] جُرّب يدويًا في التطبيق · Tried by hand in the app (لقطة شاشة للتغييرات المرئية · screenshot for visible changes)

## قائمة المراجعة · Checklist

- [ ] النصوص الجديدة في `i18n/ar.json` و`en.json` معًا · New UI strings in both `ar.json` and `en.json`
- [ ] تغييرات الأوامر موثّقة في `docs/IPC-CONTRACT.md` · Command changes documented in `docs/IPC-CONTRACT.md`
- [ ] لا اتصال جديد بالشبكة · No new network access
- [ ] أوافق على ترخيص مساهمتي بـ MIT OR Apache-2.0 · I agree my contribution is licensed MIT OR Apache-2.0
