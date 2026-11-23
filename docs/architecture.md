# Архитектура ansa

## Ответственности

Production - Cargo workspace `rust/`, crates `ansa` и `ansa-linux`. Пути в таблице
относительны `rust/crates/ansa/src/`, кроме явно указанного `ansa-linux`.

| Файлы | Ответственность |
|---|---|
| `protocol/json.rs`, `legacy_json_v1.rs`, `sse.rs` | Owning JSON, точные numeric tokens и legacy hashes, incremental UTF-8/SSE framing |
| `provider/request.rs`, `responses.rs`, `replay.rs` | Request profile, semantic admission, ordered native replay |
| `tools/registry.rs`, `admission.rs` | Immutable descriptors, full batch validation и consume-once dispatch |
| `transport/client.rs`, `http.rs` | Hyper/rustls, bounded headers/body, MIME/EOF и completion gate |
| `runtime/owner.rs`, `work.rs`, `dispatch.rs`, `outbox.rs` | Serialized state, bounded mailboxes, blocking workers, targeted Cancel |
| `session/journal_v1.rs`, `codec_v1.rs`, `state.rs`, `store.rs` | Canonical v1 journal, fsync, recovery, exact contexts и command identities |
| `session/transcript.rs`, `catalog.rs` | Производная история и выбор сессии |
| `context/` | Exact epochs, prompt summary и legacy compact codecs |
| `auth/` | SIWC/OIDC, PKCE/JWT, own private store, rotating refresh и dispatch lease |
| `process/runner.rs`, `supervisor.rs`, `ansa-linux/src/` | Process cleanup/evidence, terminal/signals и узкие Linux wrappers |
| `config/`, `tools/` | Settings, AGENTS и compiled local tools |
| `bin/ansa.rs`, `frontend/` | Production CLI/JSONL, Carbon, input и deterministic offline provider |

Cargo/Nix собирают executable. Runtime использует Tokio, Hyper/rustls/ring
и owning JSON поверх serde. Собственный unsafe запрещён в `ansa`;
узкие обоснованные Linux boundaries находятся в `ansa-linux`.

## Admission и ownership

HTTP client передаёт проверенные chunks общему Responses decoder. Успешный transfer,
финальные headers, terminal response и EOF обязательны для `AdmittedResponse`.
Offline provider использует тот же decoder и request builder. Ни chunk callback,
ни `output_item.done` не разрешают исполнение tools.

Preview ограничен 64 KiB и не является admission. `response.metadata` допускается
только с matching response ID и object metadata, проходит sequence/size checks и
не становится executable output. Added reasoning envelope может быть неполным;
только проверенное окончательное encrypted value попадает в replay.

Reducer требует created, ordered contiguous sequence и output indices, проверяет
request/response/model identities и окончательные native items. Неизвестные event/item
типы и несовпадение done/terminal отклоняются. `AdmittedResponse` имеет закрытый
constructor, владеет исходным Request и ordered native items. `PreparedBatch::prepare`
валидирует весь immutable tool snapshot и batch до executor. Rust ownership не
позволяет скопировать и повторно consume подготовленный batch.

`Json` владеет immutable `Arc` nodes и сохраняет порядок объектов/массивов и числовые
lexemes. Decoded duplicate keys и превышение depth/nodes/bytes отклоняются. Unsigned
indices разбираются отдельно; boolean, negative, fractional и overflow не принимаются.
Legacy serializer отдельно сохраняет C++ v1 dump bytes, участвующие в hashes.

## Лимиты

| Ресурс | Default |
|---|---:|
| SSE line/event и JSON event | 1 MiB |
| HTTP/SSE body и assembled request | 16 MiB |
| Function arguments | 256 KiB на call |
| Output items, включая вызовы инструментов | 64 на ответ провайдера |
| Events | 65536 |
| JSON depth / value nodes | 64 / 131072 |
| Tool result для replay | 128 KiB |
| HTTP headers / fields | 64 KiB суммарно / 256 на block |

Лимиты проверяются при incremental framing и JSON validation. JSON/SSE input bytes ограничивают объём parser allocations; depth, nodes и duplicate keys проверяются при построении owning значений. Лимит body относится к входным данным; копии owning значений увеличивают пиковую память процесса. При переполнении decoder отклоняет ответ целиком.

## Replay и provenance

`continuation` начинает с сохранённого input prefix, добавляет все native output items в прежнем порядке и затем matching function outputs. Он не принимает replacement prefix, provider, account, model или instructions. Сохраняются requested/returned model отдельно, exact epoch ID/instructions/state и snapshot revision/descriptors.

Opaque reasoning остаётся JSON value с точным encrypted string; ничего не дешифруется и не преобразуется в transcript text. Native metadata, включая неизвестные поля поддерживаемых items, сохраняется. Сравнение numeric tokens консервативное: различное написание чисел считается различием representation.

Runtime присваивает операциям `OperationIds`. Executor возвращает `ToolOutcome` без call/operation IDs. Ошибка после dispatch даёт failed/unknown; следующий call не запускается. Cancel до dispatch даёт cancelled/not_started. При cancelled/unknown результат эффекта остаётся неизвестным. Replay unknown effects блокируется до будущего reconciliation workflow.

Consume-once batch защищает конкретный подготовленный batch. Journal сохраняет принятые Submit и их identities. Runtime сверяет payload при повторе CommandId, возвращает состояние исходного turn и восстанавливает уже завершённый prefix batch с прежними operation IDs.

## Offline AgentRuntime

Один control owner меняет публичное состояние одного turn за раз. Worker строит Request и continuation, вызывает mock и проверяет весь batch перед исполнением. Storage worker строит records, применяет session reducer, восстанавливает provider responses и выполняет disk I/O. Он получает immutable owning state и возвращает новый state. Owner заменяет state после completion. Отдельный stop worker передаёт отмену blocking callbacks без выполнения их в owner. Cancel и GetSnapshot доступны во время ожидания диска и stop callbacks.

Submit FIFO вмещает 16 команд по умолчанию, ещё по одному слоту выделено для адресного Cancel, GetSnapshot и Resume. Runtime хранит до 4096 Submit ID и явно отклоняет новые при исчерпании лимита. Lookups повторных команд также ограничены размером FIFO. Storage, checkpoint и worker mailboxes имеют по одному слоту. Приём Cancel и допуск к каждому provider callback и executor сериализуются под одним mutex. Если matching Cancel уже принят, владелец обрабатывает его перед pending completion. Поздний completion отменённого turn сохраняет известные outcomes, дальнейший dispatch прекращается.

Outbox вмещает 64 события по умолчанию. Переполнение сбрасывает накопленные события и публикует `ResyncRequired`. Snapshot восстанавливает фазу, идентификаторы, состояние worker, текст, диагностику и последние 256 операций хода. Счётчик завершённых вызовов охватывает весь ход. Предыдущие операции доступны в журнале. У событий монотонный cursor в пределах incarnation. Предельный размер Submit - 16 KiB, итогового текста - 64 KiB. Счётчики вызовов и раундов по умолчанию служат для учёта. Положительный `tool_rounds` или `tool_calls` задаёт бюджет между возобновлениями. Runtime записывает выбранные значения при приёме хода и при возобновлении после паузы по бюджету. Старые записи журнала проверяются по правилам версии, которая их создала. Operation IDs продолжаются между раундами; native continuation и архив известных outcomes не теряются. Ненулевой process exit является известным результатом и допускает следующий шаг модели.

## Canonical journal и восстановление

Session directory удерживается через advisory `flock`. Новая запись проходит `fsync` до CommandAccepted, допуска executor, подтверждения outcome и Done. Создание session directory синхронизирует родителя, создание part - сам файл и session directory. Проверки inode/path/size выявляют внешнюю замену и изменение размера перед append. Эти проверки ограничены advisory locking и проверкой состояния перед записью.

Journal version 1 содержит framed JSON records с length/complement, SHA-256 и последовательной hash chain. Максимальный record - 32 MiB. Parts образуют цепочку по digest всего предыдущего файла, включая оборванные байты. Частичная последняя запись восстанавливает проверенный prefix; новый part сохраняет связь с исходными байтами. Поддерживается до 4096 parts. Существующие bytes сохраняются. Ошибка checksum, sequence, semantic replay или configuration блокирует запись и dispatch.

Session reducer хранит исходные binding, requested/returned model policy, полную epoch, tool descriptors/revisions, native items и результаты. Восстановление AdmittedResponse снова проходит через production decoder/reducer. История завершённых turns образует следующий input. Frontend snapshot остаётся производным публичным представлением.

На restart любой незавершённый turn приостановлен до Resume. Завершённый prefix tools используется с прежними operation IDs. Для незавершённого process intent recovery проверяет синхронизированное свидетельство supervisor и дописывает outcome в canonical journal. Без достоверного результата intent блокирует Resume как effect_unknown. Outcome хранит факт фактического dispatch, поэтому отменённый до executor intent не создаёт ложных ToolStarted/ToolFinished. Известный результат при storage failure остаётся в snapshot с `durable:false`. Ручное reconciliation неизвестных эффектов относится к следующей границе.

## Локальные tools и процессы

`ToolDefinition` может получать полный runtime-assigned `ToolInvocation` и recovery callback. `requires_journal` входит в snapshot revision и требует durable callbacks до любого dispatch. Process tool нельзя запустить через неперсистентный runtime или голый batch без checkpoints. Provider по-прежнему получает только descriptors; журнал и loop не содержат switch по именам tools.

ProcessRunner сохраняет private request specification, затем fork/exec запускает свежий однопоточный supervisor через тот же ELF. Между fork/exec многопоточного runtime выполняются только syscalls. Supervisor удерживает worker lock и создаёт append-only consumed marker до запуска команды. Повторный internal worker не повторяет side effect. Result связан с точными invocation/options и consumed marker по hash; missing/conflicting evidence не допускается. Atomic publication использует fsync temporary file, renameat2 NOREPLACE и fsync directory. Evidence не заменяет canonical session journal.

Supervisor сбрасывает inherited signal dispositions/mask, является subreaper, держит primary waitable до group cleanup, прекращает усыновлённых потомков включая setsid, собирает bounded stdout/stderr и точный status. Control pipe EOF означает Cancel либо гибель runtime. Timeout и Cancel прекращают дальнейший dispatch, известный итог сохраняется. Если сам supervisor уничтожен, effect остаётся unknown, повтор запуска запрещён; без cgroup гарантировать прекращение всех отделившихся потомков нельзя. PDEATHSIG прекращает primary при гибели supervisor, с проверкой исходного parent PID после установки.

Backend отклоняет direct sudo, очищает capabilities, требует non-root identity и устанавливает `no_new_privs` до exec. Флаг запрещает повышение привилегий при exec. Процесс сохраняет доступ к файлам и привилегированным сервисам, уже разрешённым пользователю. [Контракт Linux no_new_privs](https://docs.kernel.org/userspace-api/no_new_privs.html).

File tools проверяют весь batch до execution, читают только regular single-link files с nofollow walk каждого компонента. Dotenv, известные credential stores и собственные auth/session roots исключены. Search ограничивает visited entries, files, prefix bytes, matches и encoded output. Config/AGENTS также не следуют symlink/hardlink. Вложенные инструкции требуют актуального token перед доступом; shell остаётся full access и не является sandbox. Effective settings и начальные инструкции входят в exact context epoch: их смена требует новой сессии. Catalog info.json используется только для выбора сессии, canonical journal проверяет binding/config самостоятельно.

Текстовый frontend использует те же typed commands/events. Self-pipe обрабатывает SIGINT; stdout и stderr не блокируют input/control. При resync финальный текст дописывается до публикации completed локальному input loop. Pending Submit имеет локальную identity; ранний Cancel получает snapshot receipt даже во время fsync, затем отправляет targeted Cancel. EOF ждёт отправки такой отмены. Busy input явно отклоняется с exit 1, прерванный one-shot возвращает 130.

## Compact

Compact выполняется отдельной provider-операцией на границе перед новым turn. Порог учитывает JSON budget и резерв items под весь tool round и финальный response. В журнал попадают полный исходный Request и полный native result compact. После синхронизации новая exact epoch и весь returned output становятся активным контекстом.

Live compact выполняется обычным Responses-запросом с отдельным summarization prompt и пустыми tools. Старый prefix сворачивается в assistant summary; два последних завершённых turn сохраняются полностью, pending user добавляется без изменений после compact. Prompt сохраняет цель, ограничения, решения и незавершённую работу. Размер summary ограничен 16 KiB. Сохраняемый `omh.summary.v1` содержит полный admitted summary response и результирующий контекст. Recovery снова проверяет production admission, exact request provenance и равенство summary/tail. Prompt v1 является частью replay-контракта и должен оставаться стабильным при будущих форматах.

Порог live compact мягкий: до трёх завершённых turns сохраняется полный контекст. Отдельные жёсткие request/item limits продолжают действовать. При ошибке старый контекст остаётся активным, turn ждёт Resume. Native `/responses/compact` live путь не используется.

Прежний offline journal format остаётся читаемым. Его standalone result включает retained user messages и один synthetic opaque `compaction` item с encrypted_content; unknown retained executable items отклоняются. Это legacy mock-контракт, отдельный от prompt summary.

## Собственный SIWC профиль

AuthManager создаёт own registration с постоянным opaque host UUID. Первый вход использует dynamic client registration, последующие - выданный client ID. Loopback callback, свежие state/nonce и PKCE S256 проверяются до code exchange. Discovery ограничен доверенными endpoint URLs. ring проверяет RS256/JWK, signature, issuer, audience/azp, время, nonce и неизменность subject. Identity привязана к issuer, issued client и subject; workspace не выводится из email.

Auth directory и файлы проверяются по owner, permissions, type и link count. Записи проходят temporary file fsync, atomic rename и directory fsync. Один stop-aware flock сериализует обмены между процессами. JWT и access/refresh tokens находятся только в auth profile, отдельно от session journal и логов. Public binding хранит хеш identity. Identity-only scope сохраняется, но inference требует всех plan scopes. Текущая реализация ожидает полный token response с access/refresh token и expires_in, ID-token-only response без них отклоняется.

Refresh сохраняет durable pending до отправки rotating token и публикует проверенную замену после ответа. После неопределённого результата повторная отправка запрещена. Logout marker публикуется до ожидания flock, останавливает login/refresh/request и запрещает новый dispatch. Токены удаляются до попытки remote revoke. Неизвестный результат rotation/code exchange и замена активного grant при повторном входе оставляют sticky revocation uncertainty: отзыв нового grant не доказывает отзыв прежнего. Crash residue временных файлов очищается под lock; неподтверждённый revoke сохраняет это состояние между запусками.

Worker проверяет auth до provider callback и до durable tool intent, затем ещё раз после intent. Scoped lease удерживает auth lock от допуска executor до его возврата. Watcher с собственным directory fd передаёт logout stop активному executor; runtime объединяет его с targeted Cancel. Logout ждёт освобождения lease; известный outcome записывается в session journal. Отказ после intent сохраняет known not_started и приостанавливает turn. Resume удаляет только неисполненный хвост и сохраняет известные outcomes/operation IDs. Повторный вход не меняет активный auth session ID неявно: новый lease принимается только при явном Resume и прежней account binding.

401 требует входа, 403 обозначает policy denial, 429 - usage limit. Автоматического переключения аккаунта, model, billing route и повторной отправки ambiguous refresh нет. Credentials других агентов не используются.

## Carbon, transcript и режимы

`Submit.mode` закрепляет basic/plan за turn; старые записи без mode означают basic. Plan создаёт отдельную exact epoch с политикой и revision read-only snapshot. Base epoch и общая native history сохраняются, compact использует base context и затем восстанавливает режим текущего turn. Compiled tool явно объявляет `read_only`; provider получает filtered descriptors, подготовка всего batch проверяет тот же filtered snapshot. Инструкции модели не заменяют backend-проверку.

Transcript строится из проверенных canonical records: accepted добавляет user, done добавляет итоговый assistant. Cancel/pause меняют статус незавершённого запроса. Summary, opaque reasoning и tool payloads хранятся отдельно от реплик. Неизменяемые блоки по 64 записи позволяют обновлять конец проекции без копирования всех текстов. `GetTranscript` выдаёт bounded страницы до 64 записей и 256 KiB текста; frontend хранит окно страниц. Сам canonical journal по-прежнему восстанавливается полным replay.

Runtime публикует transcript только из последнего успешно синхронизированного состояния. При fsync failure candidate может сохранить известные outcomes для snapshot, но его несохранённый assistant не становится завершённой репликой. Первая страница истории загружается до первого интерактивного prompt.

Worker передаёт cumulative preview через один заменяемый mailbox slot. Serialized owner проверяет incarnation/request/resume identity и отдельный номер generation для каждого provider round. Только owner публикует `PreviewText` с состоянием preview/committed/discarded. Snapshot содержит тот же prefix и состояние для resync. Cancel и ошибка закрывают preview; поздний callback не изменяет следующую попытку. Итоговый текст snapshot доступен только для durable completed turn. Preview не записывается в journal и не используется как model context.

Терминальный frontend редактирует ввод в noncanonical режиме и перерисовывает ограниченную область основного буфера отдельным writer. Заблокированный stdout не задерживает input/control. RAII восстанавливает termios; job control временно возвращает исходные настройки. Рендерер обрабатывает ширину UTF-8 символов и обезвреживает terminal controls в пользовательском и model тексте. Pipe, one-shot и JSONL используют общий typed runtime без интерактивного рендера.

Структурированный `carbon::Entry` отделяет роль от содержимого. Markdown и fences разбираются для assistant, пользовательский текст сохраняется буквально. Диалог занимает всю высоту терминала, ввод и текущие уведомления закреплены внизу. Подтверждённый финальный preview остаётся до получения transcript page без дублирования; committed tool-response не считается завершённым turn.

Переносы, padding, курсор и копирование используют общие extended grapheme clusters и их ширину в ячейках терминала. `Frame` хранит границу панелей и ставит inspector на абсолютную колонку. Обновление справа сохраняет левую панель, включая составные emoji. Выделение хранит исходный кадр для копирования; координаты строк статуса позволяют обновлять диагностику во время выделения и в узком layout.

`Timeline` выводит `ToolDetail` из подтверждённых intent/outcome: operation/call identity, цель, назначение, фазу и границы payload. Проекция объединяет жизненный цикл вызова и группирует чтение/поиск. Неизвестные эффекты и ошибки видны в свёрнутом виде. Страница ограничена 64 entries и 256 KiB с учётом metadata; аргументы и вывод разбиваются по UTF-8 границам, части помечены явно. Snapshot дополняет операции страницы; временный вывод из него ограничен 2048 символами. Metadata сохраняется в fork seed; прежние текстовые строки читаются через совместимый fallback. Обычный v1 journal codec сохранён.

Markdown-файлы разбирает CommonMark parser в file worker. Отдельный bounded worker переносит блоки под ширину inspector и проверяет Cancel. Одновременно выполняется один такой расчёт; устаревшая работа завершается до запуска новой. Результат принимается по file token и ширине. Якорь блока и символа сохраняет место при resize; диапазон исходных строк нужен для `:номер`.

Ручной терминал занимает ту же рабочую панель, что файлы и полный вывод инструмента. Отдельный PTY worker владеет shell, файловыми дескрипторами и очисткой принадлежащей ему process session; bounded input и приоритетный interrupt отделены от runtime Cancel. Экран разбирается VT parser и рисуется через обычный renderer. Ручной ввод и вывод не становятся model context или journal events. Проверка foreground process group и zsh prompt hook разрешает Esc возврат к файлам только из приглашения shell. Скрытая панель не получает ввод. Очистка ограничена принадлежащим SID; отделившиеся через setsid процессы могут сохраниться.

Меню моделей сразу читает локальный display cache и обновляет его в worker. Этот кэш не даёт разрешения на dispatch: auth epoch и каталог отдельно проверяет authorization path. Выбор модели и effort подтверждается одной typed command. Названия сохранённых сессий выводятся из readonly replay с проверкой canonical State и идентичности файла до, во время и после чтения. Производные сводки не создают журнал и не восстанавливают его; непроверяемая сводка отмечается недоступной.

Запуск live CLI строит binding из локального `AuthStatus`, включая opaque auth session ID без bearer. `AgentAuthorization` откладывает access/refresh и каталог моделей до serialized dispatch worker. Он проверяет account/session до generation и tools, принимает смену session только при явном Resume и кеширует успешную проверку model catalog на auth epoch. `AuthManager::generate` и execution lease продолжают независимо проверять допуск. Сетевые запросы больше не предшествуют первому кадру.

## Следующая граница

Неизвестные эффекты требуют явного reconciliation workflow. File replacement отдельным compiled tool должен определить rename/preimage/conflict semantics без обещания atomic CAS с внешними writers; произвольная команда таких гарантий не даёт.

Расширение auth включает несколько профилей и системный secret store. Orchestrate, token-aware context budget, MCP, hooks, Ask, интерактивный sudo и отдельный TUI остаются в TODO.

Исходные audit/P0 reports из `/home/gusevsaint/harness-research/` использованы как справочные материалы. Репозиторий содержит синтетические fixture data.
