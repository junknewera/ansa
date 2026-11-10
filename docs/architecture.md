# Протокольный milestone

## Ответственности

| Файлы | Ответственность |
|---|---|
| `json.hpp`, `json.cpp` | Owning immutable yyjson documents, bounds, duplicate keys, finite numbers, lossless numeric tokens, fixed-code diagnostics, OpenSSL EVP digest |
| `sse.hpp`, `sse.cpp` | Incremental UTF-8/SSE framing без HTTP/provider semantics |
| `responses.hpp`, `responses.cpp` | SIWC request profile, Responses reducer, admission, ordered native replay |
| `tools.hpp`, `tools.cpp` | Compiled registry, immutable snapshot, schema/domain validation, runtime operation IDs, consume-once batch |
| `transport.hpp`, `transport.cpp` | libcurl RAII, header/MIME admission, bounded byte callbacks, provider completion gate, offline byte injection |
| `contracts.hpp` | Commands/events v1, distinct IDs, targeted Cancel и frontend snapshot/resync types |
| `main.cpp` | Dependency self-check и синтетический offline round trip |

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

Consume-once batch защищает только конкретный подготовленный batch. Между batches и после рестарта dedup должен обеспечивать будущий runtime/journal. Гарантий exactly-once и crash recovery здесь нет.

## Следующая граница

Сейчас caller сериализует работу; threads, command queue и journal отсутствуют. Следующий runtime должен иметь одного control owner, owning worker completions, priority targeted cancellation и bounded frontend outbox со snapshot/resync. Публичные commands/events отделены от provider JSON. Текущий contract header задаёт types/version checks; wire frontend и исполняемый resync ещё не реализованы.

До side-effecting tools обязательны authoritative append-only journal, durable execution intents, effect_unknown/recovery, cancellation races и process lifecycle tests. File replacement должен отдельно определять rename/preimage/conflict semantics без обещания atomic CAS с внешними writers. Здесь файловых и shell tools нет.

OAuth должен быть самостоятельным: own registration, PKCE/state/nonce, строгая ID token/JWKS validation через OpenSSL/проверенную JOSE library, identity binding, secure storage, rotating/concurrent refresh. Сейчас OpenSSL проверен как native crypto dependency; OIDC verifier и credential lifecycle не реализованы. Auth files и credentials существующих агентов не используются.

Исходные audit/P0 reports из `/home/gusevsaint/harness-research/` использованы read-only. Python probes не запускаются и не становятся dependency. Репозиторий содержит только синтетические fixture data.
