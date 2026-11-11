# oh-my-harness

Независимый C++20 runtime для персонального AI coding agent. Namespace `omh`, executable `omh`. Offline mock и собственный Sign in with ChatGPT используют общий Responses admission, pure tool, native replay и сохраняемые сессии.

Новая инженерная сессия начинается с [AGENTS.md](AGENTS.md) и [текущего состояния проекта](docs/PROJECT_STATE.md).

## Сборка и проверка

Зависимости и компилятор зафиксированы через `flake.lock`. Проверенная платформа - NixOS/aarch64, ThinkPad X13s. В flake также объявлен x86_64-linux; он пока не проверен.

```sh
nix develop --no-update-lock-file
cmake -S . -B build -G Ninja -DCMAKE_BUILD_TYPE=Debug
cmake --build build -j 4
ctest --test-dir build --output-on-failure --no-tests=error
./build/omh --self-check
./build/omh --offline-demo
```

Сборка installable Nix package с обязательными тестами:

```sh
nix build --no-update-lock-file
./result/bin/omh --self-check
./result/bin/omh --offline-demo
```

ASan/UBSan, из того же `nix develop`:

```sh
cmake -S . -B build-sanitize -G Ninja -DCMAKE_BUILD_TYPE=Debug \
  -DOMH_SANITIZERS=ON -DOMH_MUTATION_CHILD=ON
cmake --build build-sanitize -j 2
ASAN_OPTIONS=detect_leaks=1 UBSAN_OPTIONS=halt_on_error=1 \
  ctest --test-dir build-sanitize --output-on-failure --no-tests=error
```

Обычный CTest включает 56 проверок: семь protocol suites, восемь runtime, семнадцать session, пятнадцать auth, три summary, четыре проверки executable и два mutation-теста. Session suites проверяют SIGKILL на durable boundaries, журнал, Resume, дедупликацию, ошибки хранения и compact. Auth suites проверяют JWT/PKCE, refresh между процессами, гонки logout/dispatch и crash при замене credentials. Mutation-тесты меняют копии production replay builder внутри build-каталога и требуют конкретного отказа replay oracle. Ошибка компиляции mutant не считается успешным тестом. Sanitizer-конфигурация пропускает повторные сборки mutants.

Дополнительная Linux-проверка запускает JSONL с терминалом на stdin и заполняет stdout pipe. Она использует стандартную библиотеку Python.

```sh
nix develop --no-update-lock-file -c ctest --test-dir build \
  --output-on-failure --no-tests=error -R '^runtime-' --repeat until-fail:100
nix develop --no-update-lock-file -c timeout 10 python tests/cli_backpressure.py ./build/omh
```

## Вход и live JSONL

```sh
./build/omh --login
./build/omh --auth-status
./build/omh --models
./build/omh --live-jsonl --session .omh-live-session --model gpt-6.1-sol
```

Собственная регистрация использует браузер, PKCE S256, state и nonce. Если Firefox предлагает выбрать профиль, выбери его и заверши вход. CLI также печатает ссылку для ручного открытия в браузере на той же машине. Callback слушает только `127.0.0.1` и ждёт до пяти минут. Для повторного запроса разрешения использовать ChatGPT plan есть `--login-consent`.

Lifecycle следует официальным контрактам [регистрации и входа](https://developers.openai.com/siwc/token-sharing-open-source/sign-in), [профилей и сессий](https://developers.openai.com/siwc/token-sharing-open-source/profiles-and-sessions) и [токенов](https://developers.openai.com/siwc/token-sharing-open-source/token-reference).

Один профиль хранится в `$XDG_STATE_HOME/oh-my-harness` или `~/.local/state/oh-my-harness`. Каталог имеет права `0700`, файлы - `0600`. Токены хранятся в открытом виде отдельно от журнала и исходников. `--auth-dir /absolute/path` выбирает другой собственный каталог для любой auth-команды или live CLI. Credential stores других приложений не используются. `--auth-status` выводит только статус, хеш привязки учётной записи и интервалы до expiry/refresh.

Live JSONL использует команды из таблицы ниже и `session: "live"` из начального snapshot. `.omh-live-session/` исключён из Git; для offline сессии используется отдельный каталог. Каталог session связывается с выбранными account, requested/returned model и context policy. Обычный новый Submit получает сохранённую историю; смена binding требует новой сессии. `--returned-model EXACT` задаёт точную policy, если сервер возвращает имя, отличающееся от выбранного slug. По умолчанию обе строки равны. Автоматического выбора другого аккаунта или модели нет.

`--compact-bytes N` задаёт мягкий порог активного JSON-контекста (по умолчанию 262144). Live compact - обычный Responses-запрос со специальным prompt и пустым списком tools. Он создаёт сводку старой части разговора, сохраняя два последних завершённых turn и текущий запрос полностью. Сводка и её provenance синхронизируются до применения новой context epoch; полная история остаётся в журнале. Ошибка compact приостанавливает turn до Resume. До накопления трёх завершённых turns compact не применяется. Это JSON budget, оценка tokens пока не реализована.

```sh
./build/omh --refresh-auth
./build/omh --logout
```

Вход сохраняется между запусками. Rotating refresh сериализуется файловой блокировкой и выполняется перед истечением access token. Явный `--refresh-auth` также соблюдает серверный `earliest_refresh_at`; до этого времени он возвращает `auth_refresh_not_ready`. При неопределённом результате обмена токен не используется повторно, требуется новый вход. Logout немедленно запрещает новый dispatch, удаляет локальные токены и пытается отозвать refresh token. Поле `remote_revocation_confirmed` сообщает результат. Сеть недоступна, неизвестен результат предыдущего обмена или повторный вход заменил ещё активную сессию - подтверждение полного отзыва остаётся `false`. Забытые сессии можно отозвать через настройки ChatGPT.

При сетевой ошибке turn остаётся в `paused`; после устранения причины можно отправить Resume с его `request_id`. После logout и повторного входа Resume явно принимает новую auth-сессию того же аккаунта. Известные outcomes tools сохраняются, незапущенный хвост допускает безопасную повторную попытку. Уже начатый executor завершается до завершения logout. Сейчас зарегистрирован только pure `get_test_value`.

## Команды JSONL

`omh --offline-jsonl` читает одну команду на строку и отдаёт события в stdout. Первый snapshot сообщает `session` (`offline`), `incarnation` и `stream_sequence`. В каждой команде передай `schema_version: 1`, эти `session` и `incarnation`, а также `turn`, `command_id`, `type` и одно поле payload.

| `type` | Поле payload | Поведение |
|---|---|---|
| `submit` | `text` | Ставит turn в FIFO. `command_accepted` сообщает его `request_id`. Повтор той же команды даёт `command_replayed` с состоянием исходного turn. Изменённый payload с тем же ID даёт ошибку. |
| `cancel` | `target` | Используй `request_id` активного turn. Отмена проходит и при заполненном FIFO. |
| `resume` | `target` | Используй `request_id` приостановленного turn и новый `command_id`. Продолжение начинается после сохранённой безопасной границы. |
| `snapshot` | `after_sequence` | Передай последний известный cursor или `0`. Ответ содержит актуальное состояние turn и `through_sequence`. |

При разрыве `stream_sequence` или `resync_required` запроси snapshot и продолжи чтение после `through_sequence`. Читай stderr отдельно от событий. Туда идут ошибки разбора и постановки команды в очередь. EOF закрывает ввод и даёт runtime завершить текущую работу. Приостановленный turn остаётся ждать Resume.

## Сохранённая сессия

```sh
./build/omh --offline-jsonl --session .omh-session
```

Каталог создаётся с правами `0700`, journal parts - `0600`. `.omh-session/` исключён из Git. Повторный запуск с тем же каталогом восстанавливает сессию и выдаёт новую runtime incarnation. Перенеси её из начального snapshot в команды текущего подключения. Незавершённый turn получает состояние `paused` и ждёт явный `resume`. Поля `request_id`, `turn` и `resume_available` доступны в snapshot.

Журнал сохраняет историю завершённых turns, полные provider-native items, контекст и результаты tools. `command_accepted` и `done` публикуются после `fsync`. Постановка команды в FIFO сама по себе сохраняет её только в памяти. Durable Submit IDs дедуплицируются между перезапусками. Другой процесс получает `session_busy`.

Перед executor сохраняется intent, после него - outcome. Outcome синхронизируется до следующего executor. Intent без outcome после аварии получает `effect_unknown` и блокирует Resume. При ошибке записи доступны snapshot и Cancel, дальнейший dispatch останавливается. Snapshot помечает такое состояние через `storage_failed` и `durable:false`, включая известные результаты, которые не удалось сохранить.

Оборванный хвост остаётся в исходном файле, запись продолжается в следующей связанной части. Повреждение проверенной записи переводит сессию в режим просмотра с диагностикой.

## Автоматический compact

Runtime накапливает контекст между завершёнными turns. Перед новым turn он запускает compact при достижении бюджета JSON или числа items, сохраняя запас для response и tool outputs. По умолчанию бюджет составляет 256 KiB, с консервативным учётом экранирования и request profile. Это локальный offline budget. После compact новая context epoch и полный возвращённый контекст синхронизируются до generation. Полная история остаётся в append-only journal.

Ошибка compact приостанавливает turn до Resume. Offline mock возвращает retained user message и synthetic opaque compaction item. Качество сжатия реальной моделью и доступность native compact для SIWC остаются live-проверками. Контракт формы результата взят из официальной документации. [OpenAI compaction](https://developers.openai.com/api/docs/guides/compaction).

## Границы реализации

- libcurl отвечает за HTTPS bytes; bounded SSE framer - за события; Responses reducer - за semantic admission.
- Admission требует successful terminal, согласованных IDs/model/items/arguments и завершённого framing. До проверки всего batch executors недоступны.
- Owning JSON сохраняет порядок native items, opaque strings и числовые tokens без округления. Replay использует исходные prefix, context epoch и advertised tool snapshot.
- `ToolRegistry` хранит compiled descriptor/schema/validator/executor/revision. Provider получает только descriptions. В production зарегистрирован только детерминированный `get_test_value` без внешних эффектов.
- Один control owner ведёт состояние turn. Worker выполняет mock generation, подготовку и dispatch tool и строит continuation. Commands/events v1 и snapshot/resync обслуживают JSONL frontend.

CLI предоставляет self-check, offline round trip и live SIWC. Mock строит финальный response из фактического результата tool в continuation.

Сетевой adapter проверяет TLS, ограничивает размер и длительность transfer и не следует redirects. Политика MIME по умолчанию строгая; отсутствие MIME разрешается явно только для точного `POST https://api.openai.com/v1/responses`, `stream:true`, `store:false`, HTTP 200. Неверный или повторный MIME всегда приводит к отказу. Текущий live путь буферизует ограниченный HTTP body и применяет общий SSE reducer после успешного transfer; frontend пока получает текст после admission.

Поддерживается закрытое подмножество Responses events: lifecycle, function arguments, message text/refusal, reasoning summary и неисполняемый `response.metadata` с явным matching response ID. Неизвестные events/items отклоняются. Требуются `response.created`, непрерывные unsigned sequence numbers с нуля и точное совпадение returned model с явно выбранной policy. Для reasoning обязательно непустое `encrypted_content`. Это консервативный adapter, а не полная реализация всех Responses capabilities.

Для нового pure tool достаточно `ToolDefinition` и регистрации; пример с аргументами есть в suite `tools`. Поддерживаемая локальная schema: object/array/string/integer/number/boolean/null, все properties обязательны, `additionalProperties:false`; domain validation задаётся compiled validator. Неизвестные schema keywords отклоняются. При изменении реализации нужно менять `implementation_revision`.

Архитектура и ограничения: [docs/architecture.md](docs/architecture.md). Фактические gates: [docs/implementation-report.md](docs/implementation-report.md).

Request profile следует [официальному SIWC Preview contract](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations) и [models/inference contract](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference), сверенным 2026-11-11.
