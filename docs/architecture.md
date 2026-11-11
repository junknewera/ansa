# Архитектура oh-my-harness

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
| `session.hpp`, `session.cpp` | Canonical session reducer, owning codecs, history, durable command identity и восстановление Responses через production admission |
| `compaction.hpp`, `compaction.cpp` | Admission результата compact и новая exact context epoch |
| `offline.cpp` | Детерминированный mock через production framer и reducer |
| `main.cpp`, `cli.cpp` | Dependency self-check, offline demo и JSONL frontend |

Core - статическая CMake library `omh_core`. Сторонние runtime dependencies: libcurl, yyjson и libcrypto с их системной closure. Node/Python/agent CLI не участвуют в работе executable. CMake и Nix нужны для сборки.

## Admission и ownership

`SiwcProvider::generate` создаёт `ResponseStream`, передаёт ему decoded HTTP bytes и вызывает `finish` только после успешного libcurl transfer и проверки финальных headers. Offline adapter использует те же framer, reducer и request builder. Ни curl callback, ни `output_item.done` не дают разрешения на tools.

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

Outbox вмещает 64 события по умолчанию. Переполнение сбрасывает накопленные события и публикует `ResyncRequired`. Snapshot восстанавливает фазу, идентификаторы, состояние worker, текст, диагностику и результаты начатых tools последнего turn. У событий монотонный cursor в пределах incarnation. Предельный размер Submit - 16 KiB, итогового текста - 64 KiB. Допускается один tool batch на turn.

## Canonical journal и восстановление

Session directory удерживается через advisory `flock`. Новая запись проходит `fsync` до CommandAccepted, допуска executor, подтверждения outcome и Done. Создание session directory синхронизирует родителя, создание part - сам файл и session directory. Проверки inode/path/size выявляют внешнюю замену и изменение размера перед append. Эти проверки ограничены advisory locking и проверкой состояния перед записью.

Journal version 1 содержит framed JSON records с length/complement, SHA-256 и последовательной hash chain. Максимальный record - 32 MiB. Parts образуют цепочку по digest всего предыдущего файла, включая оборванные байты. Частичная последняя запись восстанавливает проверенный prefix; новый part сохраняет связь с исходными байтами. Поддерживается до 4096 parts. Существующие bytes сохраняются. Ошибка checksum, sequence, semantic replay или configuration блокирует запись и dispatch.

Session reducer хранит исходные binding, requested/returned model policy, полную epoch, tool descriptors/revisions, native items и результаты. Восстановление CompletedResponse снова проходит через production decoder/reducer. История завершённых turns образует следующий input. Frontend snapshot остаётся производным публичным представлением.

На restart любой незавершённый turn приостановлен до Resume. Завершённый prefix tools используется с прежними operation IDs. Незавершённый intent блокирует Resume как effect_unknown. Outcome хранит факт фактического dispatch, поэтому отменённый до executor intent не создаёт ложных ToolStarted/ToolFinished. Известный результат при storage failure остаётся в snapshot с `durable:false`. Ручное reconciliation неизвестных эффектов относится к следующей границе.

## Compact

Compact выполняется отдельной provider-операцией на границе перед новым turn. Порог учитывает JSON budget и резерв items под весь tool round и финальный response. В журнал попадают полный исходный Request и полный native result compact. После синхронизации новая exact epoch и весь returned output становятся активным контекстом.

Поддерживаемое подмножество standalone result включает retained user messages и один opaque `compaction` item с encrypted_content. Unknown retained executable items отклоняются. Mock сохраняет synthetic digest и последнее user message. Реальная semantic compaction, token budget и доступность SIWC endpoint пока требуют live-проверки. [Официальный контракт](https://developers.openai.com/api/docs/guides/compaction).

## Следующая граница

До side-effecting tools остаются явный reconciliation workflow и tool-specific recovery. File replacement должен отдельно определять rename/preimage/conflict semantics без обещания atomic CAS с внешними writers. Здесь файловых и shell tools нет.

OAuth должен быть самостоятельным: own registration, PKCE/state/nonce, строгая ID token/JWKS validation через OpenSSL/проверенную JOSE library, identity binding, secure storage, rotating/concurrent refresh. Сейчас OpenSSL проверен как native crypto dependency; OIDC verifier и credential lifecycle не реализованы. Auth files и credentials существующих агентов не используются.

Исходные audit/P0 reports из `/home/gusevsaint/harness-research/` использованы read-only. Python probes не запускаются и не становятся dependency. Репозиторий содержит только синтетические fixture data.
