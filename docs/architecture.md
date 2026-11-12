# Архитектура ansa

## Ответственности

| Файлы | Ответственность |
|---|---|
| `json.hpp`, `json.cpp` | Owning immutable yyjson documents, bounds, duplicate keys, finite numbers, lossless numeric tokens, fixed-code diagnostics, OpenSSL EVP digest |
| `sse.hpp`, `sse.cpp` | Incremental UTF-8/SSE framing без HTTP/provider semantics |
| `responses.hpp`, `responses.cpp` | SIWC request profile, Responses reducer, admission, ordered native replay |
| `tools.hpp`, `tools.cpp` | Compiled registry, immutable snapshot, schema/domain validation, runtime operation IDs, consume-once batch |
| `transport.hpp`, `transport.cpp` | libcurl RAII, header/MIME admission, bounded byte callbacks, provider completion gate, offline byte injection |
| `contracts.hpp` | Commands/events v1, distinct IDs, targeted Cancel и frontend snapshot/resync types |
| `runtime.cpp`, `runtime_inbox.hpp` | Serialized turn state, bounded mailboxes, dispatch и cancellation |
| `journal.hpp`, `journal.cpp` | Exclusive session lock, append-only framed records, hashes, fsync и восстановление оборванного хвоста |
| `session.hpp`, `session.cpp` | Canonical session reducer, history и durable command identity |
| `context_codec.hpp`, `context_codec.cpp` | Owning request/response codecs и восстановление Responses через production admission |
| `compaction.hpp`, `compaction.cpp` | Prompt summary, admission результата compact и новая exact context epoch |
| `auth.hpp`, `auth.cpp`, `auth_crypto.cpp` | Собственный SIWC/OIDC lifecycle, PKCE/JWT/JWKS, приватное хранилище и rotating refresh |
| `offline.cpp` | Детерминированный mock через production framer и reducer |
| `local_storage.cpp`, `process.cpp` | Приватные evidence records, отдельный process supervisor/subreaper, cancellation и recovery |
| `agent_config.cpp`, `local_tools.cpp` | Детерминированные settings, иерархия AGENTS и compiled локальные tools |
| `main.cpp`, `cli.cpp`, `auth_cli.cpp`, `agent_cli.cpp` | Текстовый CLI, dependency self-check, offline/live JSONL и команды входа |

Core - статическая CMake library `omh_core`. Сторонние runtime dependencies: libcurl, yyjson и libcrypto с их системной closure. Node/Python/agent CLI не участвуют в работе executable. CMake и Nix нужны для сборки.

## Admission и ownership

`SiwcProvider::generate` создаёт `ResponseStream`, передаёт ему decoded HTTP bytes и вызывает `finish` только после успешного libcurl transfer и проверки финальных headers. Offline adapter использует те же framer, reducer и request builder. Ни curl callback, ни `output_item.done` не дают разрешения на tools.

Live AuthManager удерживает lease профиля и logout watcher во время HTTP и передаёт проверенные транспортом chunks в общий decoder. Reducer может выдавать накопленный упорядоченный preview текста, ограниченный 64 KiB. Preview не является admission: EOF/terminal gate и полная проверка batch сохраняются. SIWC `response.metadata` допускается только с явным совпадающим response ID и объектом metadata, проходит общие sequence/size проверки и не добавляется в executable output.

Reducer требует created identity/model, последовательных sequence numbers, уникальных item/call IDs и согласованных output indices. Added/delta/done/terminal representations сверяются. Function arguments проверяются и при final-only delivery. Item order должен совпадать в done events и completed response. Для text/summary parts отдельно проверяются IDs, indices, deltas и final values. Unknown executable types и unknown events приводят к отказу.

SSE parser обрабатывает произвольные chunks, fragmented BOM/UTF-8, CR/LF/CRLF, comments, multiline data и reset event name. EOF не дописывает события. Незавершённое событие, неполный UTF-8 и semantic error от sink закрывают поток. Parser/reducer exceptions не могут быть подавлены с последующим успешным admission.

`CompletedResponse` имеет закрытый constructor и появляется только после successful terminal и EOF. Он владеет исходным `Request`, native items и thin function-call view. `PreparedBatch::prepare` принимает только этот тип, сверяет полный tool snapshot и валидирует весь batch до первого executor. Перед dispatch подготовлены operation IDs, result identities и аварийные results. Batch нельзя копировать или исполнить повторно после move/consume.

`Json` не выдаёт borrowed pointers. Значения на всех публичных границах владеют yyjson documents. Числа сохраняются как проверенные JSON numeric tokens; integer indices разбираются отдельно через `from_chars`, поэтому boolean, negative, fractional и overflowing indices не допускаются. Float tokens вне конечного диапазона double отклоняются; исходные конечные decimal tokens сохраняются без округления.

## Лимиты

| Ресурс | Default |
|---|---:|
| SSE line/event и JSON event | 1 MiB |
| HTTP/SSE body и assembled request | 16 MiB |
| Function arguments | 256 KiB на call |
| Output items / function calls | 64 / 16 |
| Events | 65536 |
| JSON depth / value nodes | 64 / 131072 |
| Tool result для replay | 128 KiB |
| HTTP headers / fields | 64 KiB суммарно / 256 на block |

Лимиты проверяются до append/parse соответствующего payload; depth проверяется также до yyjson parse. JSON/SSE input bytes ограничивают и объём внутренних parser allocations. Node/duplicate-key validation выполняется после parse. Одновременные owning copies увеличивают peak memory относительно body limit; это не обещание RSS ≤16 MiB. Overflow не приводит к усечённому executable JSON.

## Replay и provenance

`continuation` начинает с сохранённого input prefix, добавляет все native output items в прежнем порядке и затем matching function outputs. Он не принимает replacement prefix, provider, account, model или instructions. Сохраняются requested/returned model отдельно, exact epoch ID/instructions/state и snapshot revision/descriptors.

Opaque reasoning остаётся JSON value с точным encrypted string; ничего не дешифруется и не преобразуется в transcript text. Native metadata, включая неизвестные поля поддерживаемых items, сохраняется. Сравнение numeric tokens консервативное: различное написание чисел считается различием representation.

Операции идентифицирует runtime-side `OperationIds`, а не provider или executor. Executor возвращает `ToolOutcome` без call/operation IDs. Ошибка после dispatch даёт failed/unknown; следующий call не запускается. Cancel до dispatch даёт cancelled/not_started. Cancelled с unknown не превращается в отсутствие эффекта. Replay unknown effects блокируется до будущего reconciliation workflow.

Consume-once batch защищает конкретный подготовленный batch. Journal сохраняет принятые Submit и их identities. Runtime сверяет payload при повторе CommandId, возвращает состояние исходного turn и восстанавливает уже завершённый prefix batch с прежними operation IDs.

## Offline AgentRuntime

Один control owner меняет публичное состояние одного turn за раз. Worker строит Request и continuation, вызывает mock и проверяет весь batch перед исполнением. Storage worker строит records, применяет session reducer, восстанавливает provider responses и выполняет disk I/O. Он получает immutable owning state и возвращает новый state. Owner заменяет state после completion. Stop worker вызывает `request_stop()`, поскольку registered stop callbacks исполняются синхронно. Cancel и GetSnapshot доступны во время ожидания диска и stop callbacks.

Submit FIFO вмещает 16 команд по умолчанию, ещё по одному слоту выделено для адресного Cancel, GetSnapshot и Resume. Runtime хранит до 4096 Submit ID и явно отклоняет новые при исчерпании лимита. Lookups повторных команд также ограничены размером FIFO. Storage, checkpoint и worker mailboxes имеют по одному слоту. Приём Cancel и допуск к каждому provider callback и executor сериализуются под одним mutex. Если matching Cancel уже принят, владелец обрабатывает его перед pending completion. Поздний completion отменённого turn сохраняет известные outcomes, дальнейший dispatch прекращается.

Outbox вмещает 64 события по умолчанию. Переполнение сбрасывает накопленные события и публикует `ResyncRequired`. Snapshot восстанавливает фазу, идентификаторы, состояние worker, текст, диагностику и результаты начатых tools последнего turn. У событий монотонный cursor в пределах incarnation. Предельный размер Submit - 16 KiB, итогового текста - 64 KiB. Legacy frontends сохраняют default один batch; текстовый агент допускает 32 раунда и 64 calls на сегмент Resume, с абсолютными пределами 64/256 на turn. Operation IDs продолжаются между раундами; native continuation и архив известных outcomes не теряются. Ненулевой process exit является известным результатом и допускает следующий шаг модели.

## Canonical journal и восстановление

Session directory удерживается через advisory `flock`. Новая запись проходит `fsync` до CommandAccepted, допуска executor, подтверждения outcome и Done. Создание session directory синхронизирует родителя, создание part - сам файл и session directory. Проверки inode/path/size выявляют внешнюю замену и изменение размера перед append. Эти проверки ограничены advisory locking и проверкой состояния перед записью.

Journal version 1 содержит framed JSON records с length/complement, SHA-256 и последовательной hash chain. Максимальный record - 32 MiB. Parts образуют цепочку по digest всего предыдущего файла, включая оборванные байты. Частичная последняя запись восстанавливает проверенный prefix; новый part сохраняет связь с исходными байтами. Поддерживается до 4096 parts. Существующие bytes сохраняются. Ошибка checksum, sequence, semantic replay или configuration блокирует запись и dispatch.

Session reducer хранит исходные binding, requested/returned model policy, полную epoch, tool descriptors/revisions, native items и результаты. Восстановление CompletedResponse снова проходит через production decoder/reducer. История завершённых turns образует следующий input. Frontend snapshot остаётся производным публичным представлением.

На restart любой незавершённый turn приостановлен до Resume. Завершённый prefix tools используется с прежними operation IDs. Для незавершённого process intent recovery проверяет синхронизированное свидетельство supervisor и дописывает outcome в canonical journal. Без достоверного результата intent блокирует Resume как effect_unknown. Outcome хранит факт фактического dispatch, поэтому отменённый до executor intent не создаёт ложных ToolStarted/ToolFinished. Известный результат при storage failure остаётся в snapshot с `durable:false`. Ручное reconciliation неизвестных эффектов относится к следующей границе.

## Локальные tools и процессы

`ToolDefinition` может получать полный runtime-assigned `ToolInvocation` и recovery callback. `requires_journal` входит в snapshot revision и требует durable callbacks до любого dispatch. Process tool нельзя запустить через неперсистентный runtime или голый batch без checkpoints. Provider по-прежнему получает только descriptors; журнал и loop не содержат switch по именам tools.

ProcessRunner сохраняет private request specification, затем fork/exec запускает свежий однопоточный supervisor через тот же ELF. Между fork/exec многопоточного runtime выполняются только syscalls. Supervisor удерживает worker lock и создаёт append-only consumed marker до запуска команды. Повторный internal worker не повторяет side effect. Result связан с точными invocation/options и consumed marker по hash; missing/conflicting evidence не допускается. Atomic publication использует fsync temporary file, renameat2 NOREPLACE и fsync directory. Evidence не заменяет canonical session journal.

Supervisor сбрасывает inherited signal dispositions/mask, является subreaper, держит primary waitable до group cleanup, прекращает усыновлённых потомков включая setsid, собирает bounded stdout/stderr и точный status. Control pipe EOF означает Cancel либо гибель runtime. Timeout и Cancel прекращают дальнейший dispatch, известный итог сохраняется. Если сам supervisor уничтожен, effect остаётся unknown, повтор запуска запрещён; без cgroup гарантировать прекращение всех отделившихся потомков нельзя. PDEATHSIG прекращает primary при гибели supervisor, с проверкой исходного parent PID после установки.

Backend отклоняет direct sudo, очищает capabilities, требует non-root identity и устанавливает `no_new_privs` до exec. Это ограничение повышения через exec, а не OS sandbox или блокировка уже разрешённых privileged brokers. [Контракт Linux no_new_privs](https://docs.kernel.org/userspace-api/no_new_privs.html).

File tools проверяют весь batch до execution, читают только regular single-link files с nofollow walk каждого компонента. Dotenv, известные credential stores и собственные auth/session roots исключены. Search ограничивает visited entries, files, prefix bytes, matches и encoded output. Config/AGENTS также не следуют symlink/hardlink. Вложенные инструкции требуют актуального token перед доступом; shell остаётся full access и не является sandbox. Effective settings и начальные инструкции входят в exact context epoch: их смена требует новой сессии. Catalog info.json используется только для выбора сессии, canonical journal проверяет binding/config самостоятельно.

Текстовый frontend использует те же typed commands/events. Self-pipe обрабатывает SIGINT; stdout и stderr не блокируют input/control. При resync финальный текст дописывается до публикации completed локальному input loop. Pending Submit имеет локальную identity; ранний Cancel получает snapshot receipt даже во время fsync, затем отправляет targeted Cancel. EOF ждёт отправки такой отмены. Busy input явно отклоняется с exit 1, прерванный one-shot возвращает 130.

## Compact

Compact выполняется отдельной provider-операцией на границе перед новым turn. Порог учитывает JSON budget и резерв items под весь tool round и финальный response. В журнал попадают полный исходный Request и полный native result compact. После синхронизации новая exact epoch и весь returned output становятся активным контекстом.

Live compact выполняется обычным Responses-запросом с отдельным summarization prompt и пустыми tools. Старый prefix сворачивается в assistant summary; два последних завершённых turn сохраняются полностью, pending user добавляется без изменений после compact. Prompt сохраняет цель, ограничения, решения и незавершённую работу. Размер summary ограничен 16 KiB. Сохраняемый `omh.summary.v1` содержит полный admitted summary response и результирующий контекст. Recovery снова проверяет production admission, exact request provenance и равенство summary/tail. Prompt v1 является частью replay-контракта и должен оставаться стабильным при будущих форматах.

Порог live compact мягкий: до трёх завершённых turns сохраняется полный контекст. Отдельные жёсткие request/item limits продолжают действовать. При ошибке старый контекст остаётся активным, turn ждёт Resume. Native `/responses/compact` live путь не используется.

Прежний offline journal format остаётся читаемым. Его standalone result включает retained user messages и один synthetic opaque `compaction` item с encrypted_content; unknown retained executable items отклоняются. Это legacy mock-контракт, отдельный от prompt summary.

## Собственный SIWC профиль

AuthManager создаёт own registration с постоянным opaque host UUID. Первый вход использует dynamic client registration, последующие - выданный client ID. Loopback callback, свежие state/nonce и PKCE S256 проверяются до code exchange. Discovery ограничен доверенными endpoint URLs. OpenSSL проверяет RS256/JWK, signature, issuer, audience/azp, время, nonce и неизменность subject. Identity привязана к issuer, issued client и subject; workspace не выводится из email.

Auth directory и файлы проверяются по owner, permissions, type и link count. Записи проходят temporary file fsync, atomic rename и directory fsync. Один stop-aware flock сериализует обмены между процессами. JWT и access/refresh tokens находятся только в auth profile, отдельно от session journal и логов. Public binding хранит хеш identity. Identity-only scope сохраняется, но inference требует всех plan scopes. Текущая реализация ожидает полный token response с access/refresh token и expires_in, ID-token-only response без них отклоняется.

Refresh сохраняет durable pending до отправки rotating token и публикует проверенную замену после ответа. После неопределённого результата повторная отправка запрещена. Logout marker публикуется до ожидания flock, останавливает login/refresh/request и запрещает новый dispatch. Токены удаляются до попытки remote revoke. Неизвестный результат rotation/code exchange и замена активного grant при повторном входе оставляют sticky revocation uncertainty: отзыв нового grant не доказывает отзыв прежнего. Crash residue временных файлов очищается под lock; неподтверждённый revoke сохраняет это состояние между запусками.

Worker проверяет auth до provider callback и до durable tool intent, затем ещё раз после intent. Scoped lease удерживает auth lock от допуска executor до его возврата. Watcher с собственным directory fd передаёт logout stop активному executor; runtime объединяет его с targeted Cancel. Logout ждёт освобождения lease; известный outcome записывается в session journal. Отказ после intent сохраняет known not_started и приостанавливает turn. Resume удаляет только неисполненный хвост и сохраняет известные outcomes/operation IDs. Повторный вход не меняет активный auth session ID неявно: новый lease принимается только при явном Resume и прежней account binding.

401 требует входа, 403 обозначает policy denial, 429 - usage limit. Автоматического переключения аккаунта, model, billing route и повторной отправки ambiguous refresh нет. Credentials других агентов не используются.

## Carbon, transcript и режимы

`Submit.mode` закрепляет basic/plan за turn; старые записи без mode означают basic. Plan создаёт отдельную exact epoch с политикой и revision read-only snapshot. Base epoch и общая native history сохраняются, compact использует base context и затем восстанавливает режим текущего turn. Compiled tool явно объявляет `read_only`; provider получает filtered descriptors, подготовка всего batch проверяет тот же filtered snapshot. Инструкции модели не заменяют backend-проверку.

Transcript является производной проекцией проверенных canonical records: accepted добавляет user, done добавляет итоговый assistant. Cancel/pause меняют статус незавершённого запроса. Summary, opaque reasoning и tool payloads не становятся репликами. Неизменяемые блоки по 64 записи позволяют обновлять конец проекции без копирования всех текстов. `GetTranscript` выдаёт bounded страницы до 64 записей и 256 KiB текста; frontend хранит окно страниц. Сам canonical journal по-прежнему восстанавливается полным replay.

Runtime публикует transcript только из последнего успешно синхронизированного состояния. При fsync failure candidate может сохранить известные outcomes для snapshot, но его несохранённый assistant не становится завершённой репликой. Первая страница истории загружается до первого интерактивного prompt.

Worker передаёт cumulative preview через один заменяемый mailbox slot. Serialized owner проверяет incarnation/request/resume identity и отдельный номер generation для каждого provider round. Только owner публикует `PreviewText` с состоянием preview/committed/discarded. Snapshot содержит тот же prefix и состояние для resync. Cancel и ошибка закрывают preview; поздний callback не изменяет следующую попытку. Итоговый текст snapshot доступен только для durable completed turn. Preview не записывается в journal и не используется как model context.

Терминальный frontend редактирует ввод в noncanonical режиме и перерисовывает ограниченную область основного буфера отдельным writer. Заблокированный stdout не задерживает input/control. RAII восстанавливает termios; job control временно возвращает исходные настройки. Рендерер обрабатывает ширину UTF-8 символов и обезвреживает terminal controls в пользовательском и model тексте. Pipe, one-shot и JSONL используют общий typed runtime без интерактивного рендера.

## Следующая граница

Неизвестные эффекты требуют явного reconciliation workflow. File replacement отдельным compiled tool должен определить rename/preimage/conflict semantics без обещания atomic CAS с внешними writers; произвольная команда таких гарантий не даёт.

Расширение auth включает несколько профилей и системный secret store. Orchestrate, token-aware context budget, MCP, hooks, Ask, интерактивный sudo и отдельный TUI остаются в TODO.

Исходные audit/P0 reports из `/home/gusevsaint/harness-research/` использованы read-only. Python probes не запускаются и не становятся dependency. Репозиторий содержит только синтетические fixture data.
