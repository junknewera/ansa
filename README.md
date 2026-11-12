# ansa

Локальный C++20 агент с собственным многошаговым loop, чтением и поиском файлов, управляемыми командами и сохраняемыми сессиями. Запускается в любом каталоге, Git не требуется. Executable `ansa`; внутренний namespace `omh` и typed JSONL сохранены для совместимости.

Новая инженерная сессия начинается с [AGENTS.md](AGENTS.md) и [текущего состояния проекта](docs/PROJECT_STATE.md).

## Обычный CLI

```sh
./result/bin/ansa --login
./result/bin/ansa
./result/bin/ansa 'Изучи проект и исправь ошибку'
./result/bin/ansa --continue
./result/bin/ansa --session SESSION_ID
./result/bin/ansa --cwd ~/Downloads 'Найди большие файлы и объясни результат'
```

Без аргумента задачи в терминале работает Carbon CLI: диалог над строкой ввода, компактный статус tools и мятный акцент. Основной буфер терминала сохраняется; отдельное окно и alternate screen не используются. `NO_COLOR` и `TERM=dumb` отключают цвета.

При открытии видны три последних обмена и незавершённый запрос. `Ctrl+F` раскрывает и скрывает историю, `PgUp`/`PgDn` листают текст и подгружают страницы canonical journal. Compact не удаляет отображаемый диалог. Просмотр не вызывает provider или tools, набранный запрос сохраняется.

`Tab` и `Shift+Tab` переключают basic/plan между задачами; `/basic` и `/plan` делают то же. Basic выполняет задачу. Plan читает и ищет файлы, задаёт вопросы и составляет план: backend выдаёт только compiled tools с признаком `read_only`; произвольный запуск команд недоступен. Режим сохраняется с принятым turn и используется при Resume. История обоих режимов общая. Orchestrate с дочерними агентами относится к следующему спринту.

Текст live ответа появляется по мере генерации как предварительный. Полная проверка HTTP/Responses и batch по-прежнему предшествует tools; итог подтверждается после durable Done. При ошибке или отмене фрагмент остаётся с отметкой незавершённого ответа. После restart история содержит сохранённые сообщения пользователя и завершённые итоговые ответы, без временных preview, reasoning и результатов tools.

`/status` показывает состояние, `/cancel` отменяет текущий запрос, `/resume` явно продолжает прерванный turn, `/exit` завершает процесс. Ctrl+C отменяет работу, повторный Ctrl+C завершает CLI; Ctrl+D даёт принятой задаче закончиться. Одновременно выполняется одна задача. Ввод без TTY остаётся построчным; для pipeline передавай одну задачу. Перенаправленный stdout и one-shot получают проверенный итоговый ответ без интерактивного оформления.

Новая сессия создаётся при каждом запуске. `--continue` выбирает последнюю сессию текущего каталога и того же live/offline профиля; `--session` принимает её ID из стартового сообщения. Журналы находятся в `~/.ansa/sessions/`. После crash незавершённый turn ждёт `/resume`; неизвестный эффект блокирует продолжение. `--offline` проверяет CLI на фиксированном mock/get_test_value, он не эмулирует универсальную модель.

Конфигурация: `~/.ansa/config.json`, затем `<cwd>/.ansa/config.json`. Неизвестные поля и неверные типы отклоняются; `--print-config` показывает effective settings. Пример пользовательского файла:

```json
{"schema_version":1,"model":"gpt-6.1-sol","tool_rounds":32,"tool_calls":64,"process_timeout_ms":60000,"process_output_bytes":16384,"access_mode":"full_access","allow_sudo":false}
```

Проектный файл может менять model, returned_model, compact_bytes, tool_rounds, tool_calls, process_timeout_ms, process_output_bytes и exclude; security-поля разрешены только пользователю. Лимиты на сегмент Resume: до 64 раундов и 256 вызовов; такие же верхние границы действуют на весь turn. Команда ограничена десятью минутами и общим сохранённым stdout/stderr 16 KiB; обычный ненулевой exit возвращается модели как результат. Достигнутый лимит приостанавливает turn. При изменении effective config или начальных инструкций нужна новая сессия: сохранённый exact context не подменяется.

Инструкции загружаются из `~/.ansa/AGENTS.md` и `AGENTS.md` от корня до cwd. Вложенные инструкции выдаются tools до доступа к их каталогу; модель подтверждает актуальный instruction token. Автоматическая загрузка config/AGENTS не следует symlink/hardlink. Чтение файлов ограничено regular files, search имеет явные пределы и исключает вложенные instruction scopes до отдельного обращения. Dotenv, известные credential stores и собственные auth/session данные исключены из файловых tools. Произвольный shell работает с правами пользователя и не является файловой песочницей.

Full access действует без подтверждения каждой команды. Прямой sudo отклоняется; дочерние процессы получают `no_new_privs`, очищенные capabilities и stdin без терминала. Это запрещает повышение через setuid/file capabilities при exec, но не изолирует уже разрешённые пользователю привилегированные сервисы. Ask, интерактивный sudo и внешние hooks, включая TRASH, - в [TODO](docs/TODO.md).

ProcessRunner использует отдельный supervisor/subreaper: при Cancel, timeout и падении runtime он прекращает группу и усыновлённых потомков, сохраняет результат, затем освобождает ресурсы. При уничтожении самого supervisor результат считается неизвестным; убийство всех отделившихся потомков в этом случае не гарантируется без cgroup/container изоляции. Runtime не повторяет такой запуск автоматически.

## Сборка и проверка

Зависимости и компилятор зафиксированы через `flake.lock`. Проверенная платформа - NixOS/aarch64, ThinkPad X13s. В flake также объявлен x86_64-linux; он пока не проверен.

```sh
nix develop --no-update-lock-file
cmake -S . -B build-ansa -G Ninja -DCMAKE_BUILD_TYPE=Debug
cmake --build build-ansa -j 4
ctest --test-dir build-ansa --output-on-failure --no-tests=error
./build-ansa/ansa --self-check
./build-ansa/ansa --offline-demo
```

Сборка installable Nix package с обязательными тестами:

```sh
nix build --no-update-lock-file
./result/bin/ansa --self-check
./result/bin/ansa --offline-demo
```

ASan/UBSan, из того же `nix develop`:

```sh
cmake -S . -B build-ansa-sanitize -G Ninja -DCMAKE_BUILD_TYPE=Debug \
  -DOMH_SANITIZERS=ON -DOMH_MUTATION_CHILD=ON
cmake --build build-ansa-sanitize -j 2
ASAN_OPTIONS=detect_leaks=1 UBSAN_OPTIONS=halt_on_error=1 \
  ctest --test-dir build-ansa-sanitize --output-on-failure --no-tests=error
```

Обычный CTest включает protocol, runtime, session, auth, summary, многошаговый loop, локальные tools, ProcessRunner, текстовый CLI и два mutation-теста. Session suites проверяют SIGKILL на durable boundaries, журнал, Resume, дедупликацию, ошибки хранения и compact. Auth suites проверяют JWT/PKCE, refresh между процессами, гонки logout/dispatch и crash при замене credentials. Mutation-тесты меняют копии production replay builder внутри build-каталога и требуют конкретного отказа replay oracle. Ошибка компиляции mutant не считается успешным тестом. Sanitizer-конфигурация пропускает повторные сборки mutants.

Дополнительная Linux-проверка запускает JSONL с терминалом на stdin и заполняет stdout pipe. Она использует стандартную библиотеку Python.

```sh
nix develop --no-update-lock-file -c ctest --test-dir build-ansa \
  --output-on-failure --no-tests=error -R '^runtime-' --repeat until-fail:100
nix develop --no-update-lock-file -c timeout 10 python tests/cli_backpressure.py ./build-ansa/ansa
```

## Вход и live JSONL

```sh
./build-ansa/ansa --login
./build-ansa/ansa --auth-status
./build-ansa/ansa --models
./build-ansa/ansa --live-jsonl --session .omh-live-session --model gpt-6.1-sol
```

Собственная регистрация использует браузер, PKCE S256, state и nonce. Если Firefox предлагает выбрать профиль, выбери его и заверши вход. CLI также печатает ссылку для ручного открытия в браузере на той же машине. Callback слушает только `127.0.0.1` и ждёт до пяти минут. Для повторного запроса разрешения использовать ChatGPT plan есть `--login-consent`.

Lifecycle следует официальным контрактам [регистрации и входа](https://developers.openai.com/siwc/token-sharing-open-source/sign-in), [профилей и сессий](https://developers.openai.com/siwc/token-sharing-open-source/profiles-and-sessions) и [токенов](https://developers.openai.com/siwc/token-sharing-open-source/token-reference).

Новый профиль хранится в `~/.ansa/auth`. Если уже существует прежний собственный каталог `$XDG_STATE_HOME/oh-my-harness` или `~/.local/state/oh-my-harness`, ansa переиспользует его: повторный вход после переименования не нужен. Каталог имеет права `0700`, файлы - `0600`. Токены хранятся в открытом виде отдельно от журнала и исходников. `--auth-dir /absolute/path` выбирает другой собственный каталог для любой auth-команды или live CLI. Credential stores других приложений не используются. `--auth-status` выводит только статус, хеш привязки учётной записи и интервалы до expiry/refresh.

Live JSONL использует команды из таблицы ниже и `session: "live"` из начального snapshot. `.omh-live-session/` исключён из Git; для offline сессии используется отдельный каталог. Каталог session связывается с выбранными account, requested/returned model и context policy. Обычный новый Submit получает сохранённую историю; смена binding требует новой сессии. `--returned-model EXACT` задаёт точную policy, если сервер возвращает имя, отличающееся от выбранного slug. По умолчанию обе строки равны. Автоматического выбора другого аккаунта или модели нет.

`--compact-bytes N` задаёт мягкий порог активного JSON-контекста (по умолчанию 262144). Live compact - обычный Responses-запрос со специальным prompt и пустым списком tools. Он создаёт сводку старой части разговора, сохраняя два последних завершённых turn и текущий запрос полностью. Сводка и её provenance синхронизируются до применения новой context epoch; полная история остаётся в журнале. Ошибка compact приостанавливает turn до Resume. До накопления трёх завершённых turns compact не применяется. Это JSON budget, оценка tokens пока не реализована.

```sh
./build-ansa/ansa --refresh-auth
./build-ansa/ansa --logout
```

Вход сохраняется между запусками. Rotating refresh сериализуется файловой блокировкой и выполняется перед истечением access token. Явный `--refresh-auth` также соблюдает серверный `earliest_refresh_at`; до этого времени он возвращает `auth_refresh_not_ready`. При неопределённом результате обмена токен не используется повторно, требуется новый вход. Logout немедленно запрещает новый dispatch, удаляет локальные токены и пытается отозвать refresh token. Поле `remote_revocation_confirmed` сообщает результат. Сеть недоступна, неизвестен результат предыдущего обмена или повторный вход заменил ещё активную сессию - подтверждение полного отзыва остаётся `false`. Забытые сессии можно отозвать через настройки ChatGPT.

При сетевой ошибке turn остаётся в `paused`; после устранения причины можно отправить Resume с его `request_id`. После logout и повторного входа Resume явно принимает новую auth-сессию того же аккаунта. Известные outcomes tools сохраняются, незапущенный хвост допускает безопасную повторную попытку. Logout передаёт stop активному executor и ждёт освобождения его auth lease. Legacy JSONL сохраняет pure `get_test_value`; обычный CLI предоставляет локальные tools.

## Команды JSONL

`ansa --offline-jsonl` читает одну команду на строку и отдаёт события в stdout. Первый snapshot сообщает `session` (`offline`), `incarnation` и `stream_sequence`. В каждой команде передай `schema_version: 1`, эти `session` и `incarnation`, а также `turn`, `command_id`, `type` и поля payload.

| `type` | Поле payload | Поведение |
|---|---|---|
| `submit` | `text`, необязательный `mode` | Режим basic по умолчанию или plan. Ставит turn в FIFO. `command_accepted` сообщает его `request_id`. Повтор той же команды даёт `command_replayed`; другой текст или mode с тем же ID даёт ошибку. |
| `cancel` | `target` | Используй `request_id` активного turn. Отмена проходит и при заполненном FIFO. |
| `resume` | `target` | Используй `request_id` приостановленного turn и новый `command_id`. Продолжение начинается после сохранённой безопасной границы. |
| `snapshot` | `after_sequence` | Передай последний известный cursor или `0`. Ответ содержит актуальное состояние turn и `through_sequence`. |
| `transcript` | `before`, `limit`, необязательный `forward` | До 64 записей и 256 KiB текста. По умолчанию читает назад до exclusive before, 0 означает конец. При forward:true читает вперёд начиная с before, 0 означает начало. Ответ возвращает begin/end/total, entries и исходный query. |

При разрыве `stream_sequence` или `resync_required` запроси snapshot и продолжи чтение после `through_sequence`. Читай stderr отдельно от событий. Туда идут ошибки разбора и постановки команды в очередь. EOF закрывает ввод и даёт runtime завершить текущую работу. Приостановленный turn остаётся ждать Resume.

Snapshot дополнительно содержит mode, transcript_size и nullable preview с generation/text/state. Событие text_preview имеет те же поля; state принимает preview/committed/discarded. Legacy text_delta сохраняет полный подтверждённый финальный ответ. Проекция transcript содержит только пользовательский текст и durable итоговые ответы. JSONL не включает предварительный streaming по умолчанию; эти типы общие с интерактивным frontend.

## Сохранённая сессия

```sh
./build-ansa/ansa --offline-jsonl --session .omh-session
```

Каталог создаётся с правами `0700`, journal parts - `0600`. `.omh-session/` исключён из Git. Повторный запуск с тем же каталогом восстанавливает сессию и выдаёт новую runtime incarnation. Перенеси её из начального snapshot в команды текущего подключения. Незавершённый turn получает состояние `paused` и ждёт явный `resume`. Поля `request_id`, `turn` и `resume_available` доступны в snapshot.

Журнал сохраняет историю завершённых turns, полные provider-native items, контекст и результаты tools. `command_accepted` и `done` публикуются после `fsync`. Постановка команды в FIFO сама по себе сохраняет её только в памяти. Durable Submit IDs дедуплицируются между перезапусками. Другой процесс получает `session_busy`.

Перед executor сохраняется intent, после него - outcome. Outcome синхронизируется до следующего executor. При intent без outcome ProcessRunner проверяет сохранённое свидетельство результата supervisor и синхронизирует outcome в canonical journal. Если достоверного результата нет, `effect_unknown` блокирует Resume; повторного запуска команды нет. При ошибке записи доступны snapshot и Cancel, дальнейший dispatch останавливается. Snapshot помечает такое состояние через `storage_failed` и `durable:false`, включая известные результаты, которые не удалось сохранить.

Оборванный хвост остаётся в исходном файле, запись продолжается в следующей связанной части. Повреждение проверенной записи переводит сессию в режим просмотра с диагностикой.

## Автоматический compact

Runtime накапливает контекст между завершёнными turns. Перед новым turn он запускает compact при достижении бюджета JSON или числа items, сохраняя запас для response и tool outputs. По умолчанию бюджет составляет 256 KiB, с консервативным учётом экранирования и request profile. Это локальный offline budget. После compact новая context epoch и полный возвращённый контекст синхронизируются до generation. Полная история остаётся в append-only journal.

Ошибка compact приостанавливает turn до Resume. Offline mock возвращает retained user message и synthetic opaque compaction item. Live использует prompt compact, проверенный на сохранении marker после перезапуска; качество длинных сводок остаётся отдельной проверкой. Контракт формы результата взят из официальной документации. [OpenAI compaction](https://developers.openai.com/api/docs/guides/compaction).

## Границы реализации

- libcurl отвечает за HTTPS bytes; bounded SSE framer - за события; Responses reducer - за semantic admission.
- Admission требует successful terminal, согласованных IDs/model/items/arguments и завершённого framing. До проверки всего batch executors недоступны.
- Owning JSON сохраняет порядок native items, opaque strings и числовые tokens без округления. Replay использует исходные prefix, context epoch и advertised tool snapshot.
- `ToolRegistry` хранит compiled descriptor/schema/validator/executor/revision. Provider получает только descriptions. Обычный CLI регистрирует `list_directory`, `read_file`, `search_files`, `run_command` и детерминированный `get_test_value`; JSONL сохраняет прежний pure snapshot.
- Один control owner ведёт состояние turn. Worker выполняет mock generation, подготовку и dispatch tool и строит continuation. Commands/events v1 и snapshot/resync общие для JSONL и текстового frontend.

CLI предоставляет self-check, offline round trip и live SIWC. Mock строит финальный response из фактического результата tool в continuation.

Сетевой adapter проверяет TLS, ограничивает размер и длительность transfer и не следует redirects. Политика MIME по умолчанию строгая; отсутствие MIME разрешается явно только для точного `POST https://api.openai.com/v1/responses`, `stream:true`, `store:false`, HTTP 200. Неверный или повторный MIME всегда приводит к отказу. Production live путь подаёт chunks в общий SSE reducer во время transfer. Preview предварительный; `CompletedResponse` появляется только после successful transfer, terminal и EOF. Тестовый injectable buffered adapter сохраняется.

Поддерживается закрытое подмножество Responses events: lifecycle, function arguments, message text/refusal, reasoning summary и неисполняемый `response.metadata` с явным matching response ID. Неизвестные events/items отклоняются. Требуются `response.created`, непрерывные unsigned sequence numbers с нуля и точное совпадение returned model с явно выбранной policy. Для reasoning обязательно непустое `encrypted_content`. Это консервативный adapter, а не полная реализация всех Responses capabilities.

Для нового pure tool достаточно `ToolDefinition` и регистрации; пример с аргументами есть в suite `tools`. Поддерживаемая локальная schema: object/array/string/integer/number/boolean/null, все properties обязательны, `additionalProperties:false`; domain validation задаётся compiled validator. Неизвестные schema keywords отклоняются. При изменении реализации нужно менять `implementation_revision`.

Архитектура и ограничения: [docs/architecture.md](docs/architecture.md). Фактические gates: [docs/implementation-report.md](docs/implementation-report.md).

Request profile следует [официальному SIWC Preview contract](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations) и [models/inference contract](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference), сверенным 2026-11-11.
