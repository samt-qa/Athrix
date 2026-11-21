# الإصدار والتوزيع (المهمة 5.7)

## بناء المثبّت محليًا
```bash
cd apps/desktop
npm ci
npm run tauri build        # يجهّز عامل PDF (release) ثم يبني التطبيق والمثبّت
```
الناتج: `target/release/bundle/nsis/Athrix_<version>_x64-setup.exe`.

- **NSIS فقط** (`bundle.targets = ["nsis"]`): مثبّت لكل مستخدم (`installMode: currentUser`) في
  `%LOCALAPPDATA%\Athrix` بلا صلاحيات مدير (ADR-008: لا `requireAdministrator` أبدًا). أُسقط MSI لأن مثبّت WiX في Tauri
  لكل الجهاز ويطلب صلاحيات مدير.
- لغتا المثبّت: العربية والإنجليزية، وتُختار تلقائيًا حسب لغة ويندوز.
- يُنشئ اختصارًا في قائمة ابدأ، ويضم عامل PDF (`athrix-extract-worker.exe`) بجانب التطبيق.
- خطافات المثبّت في `apps/desktop/src-tauri/windows/hooks.nsh`: تسجّل «ابحث هنا بـ Athrix» في قائمة الزر الأيمن
  (HKCU) وتحذفه عند الإزالة. في ويندوز 11 يظهر تحت «إظهار المزيد من الخيارات». الملف يجب أن يبقى UTF-8 **مع BOM**.
- الهدف < 15MB (docs/01 §10).

## توقيع الشيفرة (Authenticode)
لا توجد شهادة بعد، فالمثبّت غير موقَّع (سيظهر تحذير SmartScreen). الإعداد جاهز وغير مفعَّل:

1. احصل على شهادة توقيع شيفرة (OV أو EV، أو Azure Trusted Signing).
2. عرّف متغيرات البيئة (لا تُحفظ في المستودع أبدًا):
   - `ATHRIX_SIGN_THUMBPRINT`: بصمة SHA-1 لشهادة مثبّتة في `Cert:\CurrentUser\My`، **أو**
   - `ATHRIX_SIGN_PFX` و `ATHRIX_SIGN_PFX_PASSWORD`: ملف `.pfx` وكلمة مروره.
   - اختياري: `ATHRIX_SIGNTOOL` (مسار `signtool.exe`)، `ATHRIX_TIMESTAMP_URL` (الافتراضي DigiCert).
3. ابنِ مع ملف الإعداد الإضافي الذي يفعّل `signCommand`:
   ```bash
   npm run tauri -- build --config src-tauri/windows/signing.conf.json
   ```
   يستدعي Tauri السكربت `src-tauri/windows/sign.ps1` لكل ملف (التطبيق، العامل، المثبّت). لم يُختبر بشهادة حقيقية بعد.
4. في CI: أضف الشهادة أسرارًا واستدعِ الأمر نفسه في `release.yml` (أو استبدل السكربت بـ Azure Trusted Signing).

## مفتاح توقيع التحديثات
التحديثات موقَّعة بمفتاح minisign مستقل عن Authenticode، وإلزامية: التطبيق يرفض أي تحديث لا يطابق المفتاح العام.

- **المفتاح الخاص**: `%USERPROFILE%\.athrix\updater.key` على جهاز المطوّر (خارج المستودع)، **بلا كلمة مرور**.
  المفتاح العام بجانبه (`updater.key.pub`) ومنسوخ في `tauri.conf.json` ⇠ `plugins.updater.pubkey`.
- **احفظ نسخة احتياطية آمنة منه** (مدير كلمات مرور مثلًا). فقدانه يعني أن النسخ المثبّتة لن تقبل أي تحديث بعده
  (يلزم توزيع مثبّت جديد يدويًا بمفتاح عام جديد).
- إن أردت كلمة مرور: أعد التوليد بـ `npx tauri signer generate -w <path> -p <password>` وحدّث `pubkey`، وضع كلمة المرور في
  السر `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`.
- أسرار GitHub المطلوبة: `TAURI_SIGNING_PRIVATE_KEY` = **محتوى** الملف `updater.key`، و
  `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` = فارغ.
- بناء محلي مع ملفات التحديث (`.sig`):
  ```bash
  TAURI_SIGNING_PRIVATE_KEY="$USERPROFILE/.athrix/updater.key" TAURI_SIGNING_PRIVATE_KEY_PASSWORD="" \
    npm run tauri -- build --config '{"bundle":{"createUpdaterArtifacts":true}}'
  ```
  (`createUpdaterArtifacts` مطفأ في الإعداد الأساسي كي لا يحتاج كل بناء محلي إلى المفتاح.)

## نشر إصدار
1. ارفع الإصدار في `Cargo.toml` (`workspace.package.version`) و `tauri.conf.json` و `apps/desktop/package.json`.
2. أنشئ وسمًا `vX.Y.Z` وادفعه. سير العمل `.github/workflows/release.yml` يبني على `windows-latest` بـ `tauri-action`،
   ويوقّع ملف التحديث، وينشئ **مسودة** إصدار فيها المثبّت و `.sig` و `latest.json`.
3. راجع المسودة وانشرها. بعد النشر يصبح `releases/latest/download/latest.json` هو ما يسأله التطبيق.

## التحديث التلقائي والخصوصية
- لا اتصال بالإنترنت افتراضيًا: الفحص يدوي (`update_check`)، والفحص عند الإقلاع مطفأ (`autoUpdateCheck = false`)
  إلا إن فعّله المستخدم. التثبيت دائمًا بطلبه (`update_install`). التفاصيل في `docs/IPC-CONTRACT.md`.
- على ويندوز يُشغَّل المثبّت الجديد في الوضع `passive` (شريط تقدم بلا أسئلة) ثم يُعاد تشغيل التطبيق.
- العنوان: `https://github.com/samt-qa/Athrix/releases/latest/download/latest.json`.
  **تنبيه**: المستودع خاص، وملفات إصدارات المستودع الخاص لا تُنزَّل دون مصادقة، فالفحص سيفشل حتى تصبح الإصدارات عامة
  (مستودع عام للإصدارات فقط مثلًا) أو يُستخدم خادم آخر (غيّر `plugins.updater.endpoints`، ويجب أن يبقى المفتاح العام نفسه).
