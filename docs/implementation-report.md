# Отчёт о milestones

## Локальный многошаговый агент ansa

2026-11-11, `x13s`, NixOS/aarch64, GCC 16.2.0. Код - [66250f7](https://github.com/junknewera/oh-my-harness/commit/66250f78498f484e904c7261e9c173c0852f4fc7). Собственный loop расширен на несколько tool rounds; добавлены local tools, ProcessRunner и текстовый CLI. Переименование не меняет внутренний namespace, legacy JSONL или сохранённый auth profile.

| Gate | Результат |
|---|---|
| Debug CMake/CTest | PASSED **81/81**, 155.11 s, включая прежние protocol suites и оба replay mutants. |
| ASan/UBSan | PASSED **79/79**, 32.86 s, `detect_leaks=1:halt_on_error=1`, `UBSAN_OPTIONS=halt_on_error=1`. Mutant rebuilds выключены. |
| Native Nix package | PASSED **81/81**, Release, CTest 123.57 s. Установленный ELF64 AArch64 проходит self-check, offline demo и использует сохранённый вход. |
| Повторы concurrency | PASSED **200/200**: шесть loop/process/auth suites и четыре CLI suites по 20 повторов. |
| Production CLI | PASSED: UTF-8, fresh/continue/explicit session, status/exit, EOF, отказ при exact config mismatch. |
| Live без Git | PASSED: `list_directory → read_file → run_command(exit 7) → run_command(exit 0) → read_file → Done`. Marker и файл результата проверены; durable process evidence подтверждает два запуска и завершённый cleanup. |
| Повторный live запуск | PASSED: тот же session ID и сохранённый вход, новый turn вернул marker предыдущего разговора без tools; второй durable Done подтверждён в canonical journal. |
| Независимые review | COMPLETE: runtime reviewer **10/10 + 4/4**, security reviewer **26/26**. Все P1/P2 исправлены; `git diff --check` чистый. |

Новые regression suites проверяют уникальные operation IDs между раундами, known failed outcomes, bounded rounds с явным Resume, crash на границах второго раунда и Cancel после второго durable response. Process recovery проверен реальным subprocess: runtime падает после команды до canonical outcome, startup подхватывает синхронизированный artifact, Resume не повторяет команду. Ошибки recovery append как `Error`, так и `std::exception` оставляют known outcome видимым с durable:false и запрещают dispatch.

Process tests покрывают stdout/stderr flood и некорректные UTF-8 bytes, timeout, Cancel, crash runtime, setsid descendants, ненулевой exit, NUL cwd, inherited SIGCHLD/SA_NOCLDWAIT, повторный и параллельный internal worker, consumed launch без результата, missing/conflicting evidence marker и SIGKILL supervisor. Неизвестный результат не запускается повторно.

Независимый adversarial review выявил и закрепил регрессиями: повторный запуск internal worker, ложный status при inherited SIGCHLD, отсутствие marker binding у evidence, утечку credentials через ссылки AGENTS/file tools, некорректный parent-death setup и две гонки CLI. Полный stdout при snapshot fallback не выключает обработку SIGINT; прежний порядок воспроизведён как зависающий mutant. Ранний `/cancel` при blocked acceptance fsync проверяет **ноль generation dispatch**. Busy multi-line input и overflow незавершённой строки возвращают ошибку. Logout передаёт stop исполнителю, ждёт lease и не требует живого AuthManager для lifetime watcher.

Границы: full access не является sandbox; no_new_privs блокирует exec elevation, но не уже разрешённые privileged brokers. Уничтожение supervisor оставляет unknown и не гарантирует прекращение всех отделившихся потомков без cgroup. Config/начальные инструкции привязаны к exact epoch, их изменение требует новой сессии. Reconciliation unknown effects, streaming, MCP, hooks, Ask, interactive sudo и TUI отложены. Реальные credentials и acceptance journal находятся вне Git.

## Первый protocol milestone

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

## Offline AgentRuntime

Код `10a11dd`, X13s, NixOS/aarch64, GCC 16.2.0.

| Gate | Результат |
|---|---|
| Native Debug CMake и Release Nix package | CTest **20/20** в каждой сборке, включая обе replay mutations. Установленный ELF64 AArch64 проходит self-check, offline demo и JSONL handshake. |
| ASan/UBSan | **18/18** с leak detection и остановкой на UB. Сборки mutants исключены через `OMH_MUTATION_CHILD=ON`. |
| Runtime adversarial | **800/800** повторов восьми runtime suites. Детерминированные барьеры для blocked generation, stop callback и Cancel перед completion. Проверены отсутствие dispatch после отмены, сохранение результата запущенного tool, shutdown, stale incarnation, duplicate Submit, queue overflow и snapshot/resync. |
| Provider admission | Failed, incomplete, malformed, premature EOF и ошибка schema второго call в batch дают ноль executor invocations. Успешный turn использует результат pure tool в production continuation. |
| Independent review | Отдельный агент проверил сериализацию Cancel/dispatch/completion, stop worker, владение результатами и transport backpressure. Найденные проблемы закрыты регрессиями для blocked callback, valueless command, resync с результатом и PTY с заполненным stdout. |
| PTY и Git | `tests/cli_backpressure.py` прошёл на Debug executable. `git diff --check` прошёл. |

Границы и команды воспроизведения - в [PROJECT_STATE](PROJECT_STATE.md), [архитектуре](architecture.md#offline-agentruntime) и [README](../README.md).

## Persistent sessions, recovery и compact

Код `c0295bd`, X13s, NixOS/aarch64, GCC 16.2.0.

| Gate | Результат |
|---|---|
| Native Debug CMake и Release Nix package | CTest **37/37** в каждой сборке. Установленный executable проходит self-check, offline demo и persistent JSONL через два запуска. |
| ASan/UBSan | **35/35**, detect_leaks=1 и halt_on_error=1, без сборок mutants. |
| Crash recovery | SIGKILL после acceptance, response, intent, outcome, continuation, Done и compact. Resume сохраняет native context и operation IDs, завершённый prefix batch не исполняется повторно. Intent без outcome блокирует Resume. |
| Storage errors | Failed write/sync, blocked disk, late known outcome после failed Cancel, advisory lock между процессами, внешнее изменение файла, torn tail и partial part header, повреждённый checksum, parts limit. Snapshot/Cancel остаются доступны. |
| Compact | Автоматический JSON/item budget, durable epoch, restart после compact, Cancel во время compact, отказ и Resume, oversized output. Mock проверяет форму и lifecycle synthetic native state. |
| Concurrency | Пять новых suites по 20 повторов, **100/100**. PTY regression с заполненным stdout также прошёл. |
| Independent review | Отдельный агент проверил concurrency/storage/replay. Исправлены все findings, добавлены соответствующие regressions. Финальный focused review без открытых замечаний. |
| Filesystem и Git | strace подтвердил fsync родителя нового session directory, part и session directory. `git diff --check` прошёл. |

Power-cut hardware tests, TSan, x86_64-linux, длительные сессии, semantic quality и доступность live SIWC compaction остаются открытыми. Доступен closed offline compaction subset с synthetic encrypted item. Полный scope, лимиты и следующий шаг записаны в [PROJECT_STATE](PROJECT_STATE.md).

## Собственный SIWC и live provider

Проверенный код `e2f8e1c`, 2026-11-11, NixOS/aarch64, GCC 16.2.0. Milestone закрыт, включая реальный rotating refresh и live turn после обновления входа.

| Gate | Результат |
|---|---|
| Native Debug и Release Nix package | **56/56** CTest в каждой конфигурации, включая оба replay mutants. Установленный AArch64 executable проходит self-check и offline demo. |
| ASan/UBSan | **54/54**, detect_leaks=1, halt_on_error=1, без сборок mutants. |
| Собственный OAuth | Реальный browser login, issued registration, JWT/JWKS validation и account catalog прошли. Вход сохраняется после запуска нового процесса. |
| Live turns | Шесть завершённых synthetic turns. В первом и четвёртом - model → get_test_value → native continuation → Done. Повтор Submit после restart даёт command_replayed без повторного turn. Шестой turn после refresh завершён ответом REFRESH_OK. |
| Prompt compact и recovery | Три реальные сводки через обычный Responses prompt. Два последних завершённых turn сохраняются точно; marker старого prefix остаётся после compact и restart. После transport failure пятый turn восстановлен Resume и завершён. |
| Auth adversarial | 15 suites: RS256/JWK/claims, PKCE RFC oracle, permission, private storage, crash residue, rotating refresh между процессами, pending crash, refresh scope loss, logout races и held executor lease. Повторный вход с двумя разными grants не создаёт ложного обещания их общего отзыва. |
| Summary adversarial | Provenance и exact tail, durable recovery, late cancelled summary, пустой/oversized/refusal response и подмена output. Отказ summary сохраняет прежний контекст. |
| Concurrency stress | Семь suites по 20 повторов, **140/140**: process-refresh, dispatch/logout, logout/login, logout/refresh, held executor lease, post-intent dispatch guard и late cancelled summary. |
| Independent review | Отдельные runtime и security reviewers. Закрыты findings о dispatch после logout, guard после intent, crash identity binding, credential residue, ambiguous refresh и достоверности revoke receipt. Все исправления закреплены регрессиями; финальные focused review без blockers. |
| PTY и Git | Backpressure regression прошла. Working и staged diff проверены, diff --check чистый. |
| Реальный refresh | PASSED около 14:00 МСК после earliest_refresh_at. Обмен refresh_token прошёл, проверенные credentials заменены атомарно. Новый expires_in - 3600, каталог из восьми моделей доступен, последующий live turn завершён без повторного входа. |

В live probe найден `response.metadata` с explicit response_id. Reducer принимает bounded object metadata, сохраняя sequence/identity/terminal gates. Событие не добавляет executable items; negative regressions проверяют malformed identity/shape и failed terminal. Неподдерживаемые events по-прежнему отклоняются.

Реальные токены, callback codes и live journal не включены в Git или fixtures. Live encrypted reasoning отдельно не наблюдался; сохранение opaque bytes проверяется synthetic protocol/replay tests. Качество больших summary, token-aware budget, streaming frontend, hardware power loss, TSan и другие платформы остаются ограничениями. Remote revoke проверен на synthetic server; реальная logout-приёмка отдельно не проводилась.

## Carbon CLI

Проверенный код `93393ab`, 2026-11-12, `x13s`, NixOS/aarch64, GCC 16.2.0. Milestone закрыт: Carbon, восстановление диалога, настоящий streaming и durable basic/plan с переключением Tab. Следующий согласованный спринт - orchestrate.

| Gate | Результат |
|---|---|
| Native Debug и Release Nix package | **87/87** CTest в каждой конфигурации, включая replay mutants. Release derivation `r6pn7lch16l8vrwd1p1mddl3m632xmvw-ansa-0.1.0.drv`; установленный ELF64 AArch64 проходит self-check и offline roundtrip. |
| ASan/UBSan | **85/85**, detect_leaks=1 и halt_on_error=1, без сборок mutants. |
| Concurrency stress | Пять frontend suites и PTY terminal по 20 повторов, **120/120** на финальном коде. Дополнительные targeted прогоны при исправлении гонок не включены в этот итог. |
| History и replay | Accepted user и durable Done assistant, прежний basic journal без mode, общий диалог basic/plan, restart и compact. Immutable blocks по 64 записи, byte-limited страницы в обоих направлениях, query echo и первая история до prompt. |
| Streaming admission | Preview до terminal; invalid terminal/batch, oversized text, late callback, Cancel, storage failure и overflow. Ноль executors при отвергнутом batch. Snapshot не возвращает отменённый ответ; fsync failure Done не публикует несохранённый assistant в transcript. |
| Plan | Только read_only descriptors и соответствующий backend snapshot. Mixed safe/write batch целиком отклоняется до executors. Mode входит в durable Submit identity, exact epoch и Resume; последовательность basic → plan → basic сохраняет историю. |
| Terminal | PTY с outbox из двух событий и 300 preview, sticky overflow, busy input preservation, Ctrl+F/PgUp/PgDn, Tab/Shift+Tab, resize, SIGINT/SIGTERM и восстановление termios. Кириллица, emoji, combining и некорректные bytes проверены при LC_ALL=C без LOCALE_ARCHIVE. Отдельная cli_backpressure.py прошла через pinned Nix Python. |
| Independent review | Native GPT-6.1 Terra low: независимые runtime/provider/session и CLI/TTY reviewers. Все подтверждённые findings исправлены, финальные source review закрыты. Реализацию также выполняли native Terra low workers. |
| Live | Terra в каталоге без Git: plan list_directory/read_file → PLAN_OK без изменений; basic после ошибки transport и явного Resume → run_command → BASIC_OK, файл совпадает с marker. Реальный текст был виден частями до завершения. Ctrl+F/PgUp/PgDn сохраняют черновик. |
| Restart | --session и --continue восстановили обе реплики и basic. Финальный --continue показал историю до первого prompt. Открытие не продолжило завершённый turn. |
| Git | Working и staged diff проверены, diff --check чистый. Пользовательский designs/ сохранён вне checkpoint. Credentials и synthetic live journal вне репозитория. |

Открыты прежние hardware power loss, TSan, x86_64-linux и длительные сессии; основной reducer всё ещё копирует submit map. Ручной reconciliation неизвестного эффекта, качество длинных summary и token-aware budget остаются отдельными задачами. Plan намеренно не допускает произвольные команды; preview хранится только в живом frontend state, после restart доступен подтверждённый transcript.
