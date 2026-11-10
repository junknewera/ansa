# Отчёт о первом milestone

Срез: 2026-11-11, `x13s`, NixOS/aarch64. Реализованы `omh_core` и `omh`, bounded SSE/JSON, Responses admission, compiled pure tool и production request/replay builder. Audit и P0 использованы как read-only references.

| Gate | Результат |
|---|---|
| Native compile/link/run | PASSED. GCC 16.2.0, ELF64 AArch64 по `readelf`. Debug CMake и Release Nix package. |
| Dependencies | PASSED. libcurl 8.22.0 с OpenSSL TLS, yyjson 0.12.0 и OpenSSL/libcrypto 3.5.8. `ldd` подтверждает native shared linkage curl/crypto, `nm` - статический yyjson; `--self-check` выполняет curl handle/URL init, JSON parse и EVP SHA-256 known-answer test. |
| Framing и bounds | PASSED. Все 1923 single-split позиции production fixture, 32 seeded partitions, bytewise BOM/UTF-8 и CR/LF/CRLF, multiline/comments/reset, invalid UTF-8, EOF и size limits. |
| Adversarial admission | PASSED. 87 отклонений reducer, 13 HTTP/MIME и 2 batch/schema; на каждом отказе счётчик executor не меняется. Включены orphan IDs, conflicting identities/arguments, invalid numbers, wrong model, unknown tools/items, final-only overflow, failed/incomplete/trailing partial stream. |
| Native replay | PASSED. `get_test_value` выполняется один раз в offline demo. Следующий production request сохраняет исходный prefix, ordered items, opaque bytes, matching call ID, exact context epoch и tool snapshot. Второй synthetic response завершается `OMH_TOOL_OK:OMH_FIXED_VALUE_42`. |
| Mutation sensitivity | PASSED. Обе копии production builder компилируются. Тест `replay` падает на item count при удалении и на item order при перестановке. |
| Tool extension и cancellation | PASSED в памяти. Test-only `echo_test_value` зарегистрирован без изменения provider/reducer/executor loop. Проверены pre-dispatch cancellation, consume-once batch, post-dispatch exception/unknown effect и блокирование replay. |
| Frontend boundary | PASSED для contract types: команды/события v1, раздельные IDs, targeted Cancel, snapshot/resync payloads, rejection неизвестной версии. |
| CTest | PASSED, 11/11, включая mutation tests. ASan/UBSan: 9/9 без повторной сборки mutants. Пустой test tree с `--no-tests=error` возвращает 8. |

Непроверенные gates за пределами этого milestone:

- Реальные HTTPS/TLS transfer, серверные вариации SIWC и encrypted reasoning replay. `omh` не выполнял live OAuth/model requests. P0 live evidence относится к предыдущим probes; текущие fixtures синтетические.
- F04-F06: регистрация, PKCE/state/nonce, ID token/JWKS, secure storage и refresh lifecycle. Проверена libcrypto, не OIDC.
- F09-F10 и F13: crash recovery, durable intents, side effects и file replacement. In-memory unknown effects обработаны; журнал и filesystem/shell executors не добавлены.
- F12: реальные frontend queues, snapshot/resync transport и cancellation latency при зависшем worker. Граница зафиксирована в типах, runtime ещё не запущен.
- x86_64-linux, долгие сессии, модельное качество и расход quota не проверялись.

Команды воспроизведения - в [README](../README.md). Политика replay и границы поддержки - в [architecture.md](architecture.md).
