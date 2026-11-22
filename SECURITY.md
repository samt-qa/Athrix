# سياسة الأمان · Security Policy

## الإصدارات المدعومة
أثر قيد التطوير ولم يصدر منه إصدار مستقر بعد؛ الإصلاحات الأمنية تُطبَّق على أحدث كود في الفرع `main`، وعلى آخر إصدار منشور بعد بدء الإصدارات.

## الإبلاغ عن ثغرة
**لا تفتح Issue عامًا للثغرات الأمنية.** أبلغ بشكل خاص عبر [Security Advisories](https://github.com/samt-qa/Athrix/security/advisories/new) في GitHub، واذكر:
- وصف الثغرة وأثرها؛
- خطوات إعادتها أو مثالًا عليها؛
- إصدار أثر ونظام التشغيل.

سنؤكّد الاستلام خلال 7 أيام، ونعمل على الإصلاح، ونذكرك في ملاحظات الإصدار إن رغبت.

## ما يهمّنا خصوصًا
أثر يقرأ ملفات المستخدم ويفهرسها، لذلك تهمّنا بشكل خاص:
- أي طريقة تجعل محتوى الملفات أو أسماءها يخرج من الجهاز؛
- ملفات خبيثة (PDF أو Office أو أرشيفات) تُسقط البرنامج أو تنفّذ كودًا أثناء الاستخراج أو المعاينة؛
- تجاوز قاعدة «الواجهة لا ترسل مسارات» لفتح أو قراءة ملفات خارج النتائج؛
- مشاكل في آلية التحديث أو توقيعه.

---

## English
Athrix has no stable release yet; security fixes land on `main` (and on the latest published release once releases start). **Do not open public issues for vulnerabilities.** Report them privately through [GitHub Security Advisories](https://github.com/samt-qa/Athrix/security/advisories/new) with a description, reproduction steps, and your Athrix version and OS. We aim to acknowledge reports within 7 days. Of particular interest: data leaving the machine, malicious documents crashing or executing code during extraction or preview, bypassing the "the UI never sends paths" rule, and update-signing issues.
