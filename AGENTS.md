# AGENTS.md

`ansa` - минимальный agent runtime, который легко читать, менять и расширять. Production runtime - Rust с executable `ansa`; namespace `omh` и legacy v1 codecs сохранены для совместимости. Agent loop принадлежит проекту и не зависит от Codex SDK или app-server.

M9 прошёл bounded live acceptance. Пользователь отдельно разрешил M10: C++ production sources и CMake удаляются, default/`.#ansa-rust` собирают Rust. Проверенный C++ rollback сохранён вне production build graph. Текущие gates - [M10](docs/migration/M10.md), результаты - [PROJECT_STATE](docs/PROJECT_STATE.md).

## Начало задачи

1. Прочитай этот файл и [docs/PROJECT_STATE.md](docs/PROJECT_STATE.md). Проверь `git status --short --branch` и последние коммиты. Для migration прочитай также [план](docs/RUST_MIGRATION_PLAN.md) и `rust/AGENTS.md`, если он существует.
2. Уточни acceptance criteria по задаче и изучи относящиеся к ним исходники и тесты. [Architecture](docs/architecture.md) объясняет границы. Исследования из PROJECT_STATE нужны для конкретного вопроса и доступны только для чтения.
3. Сделай минимальный diff и сохрани чужие и незавершённые изменения. Архитектурный blocker устрани до новых функций либо запиши, почему он отложен и какую проверку блокирует.
4. Проверяй результат реальной сборкой и тестами. Молчание пользователя не подтверждает корректность. Не возобновляй фундаментальное исследование без нового вопроса.

## Рабочий цикл и стоимость проверок

1. Во время реализации запускай сборку и целевые проверки изменённых участков.
   Для runtime-изменений закрой замечания независимого review до финальной
   Nix-сборки, ASan и полной acceptance-матрицы. Передавай reviewer текущий diff,
   критерии и результаты регрессий; исправления проверяй адресно.
2. Перед выбранными suites выполни один общий preflight подготовки: нужный
   toolchain, добавленные в Git новые modules, C++/Rust binaries и adapters,
   соответствие CMake cache текущему source path и build profile, требования
   evidence к именам, типам, hashes и числу hard links. Недостающие prerequisites
   подготовь до capture; при ошибке подготовки не запускай весь набор.
   Используй новые уникальные output directories и Python `-B` либо
   `PYTHONDONTWRITEBYTECODE=1`. Если adapter нужен как самостоятельный файл,
   создай проверенную копию, сохранив исходник и его hash; не ослабляй guards.
3. После исправлений и review зафиксируй чистый code checkpoint. Полные gates
   выполняй на этом SHA без редактирования worktree. Документацию с результатами
   обновляй после capture. Новый finding возвращает задачу к исправлению,
   целевой регрессии и review, затем к новому checkpoint; failed attempts сохраняй.
4. На одной машине выполняй одну тяжёлую сборку за раз с общим лимитом jobs.
   Не запускай Cargo, Nix и CMake builds параллельно друг другу либо с PTY,
   fault, sanitizer или performance acceptance. Сначала закончи компиляцию,
   затем дай чувствительным к времени suites отдельное окно без конкурирующей
   нагрузки. Нагрузочный сценарий запускай явно, фиксируй нагрузку и отдельный
   результат; тихий PASS не закрывает наблюдавшийся отказ под нагрузкой.
5. В evidence записывай SHA, commands, время начала/окончания и длительность
   фаз, ожидание очереди и категории ошибок: setup, oracle, product, source-change.
   Не складывай длительности параллельных команд как время всей задачи и не
   приписывай неизмеренные паузы reasoning или выбранной модели.
6. Полные обязательные gates сохраняются. Повторяй их после изменения inputs,
   нового finding или сбоя, а не после каждого WIP. Для небольшой docs-only
   правки достаточно diff/check, проверки ссылок и согласованности инструкций;
   неизменный runtime не требует новой сборки и ASan. Сохраняй build caches между
   итерациями; cleanup выполняй отдельно, не удаляя evidence и rollback binaries.

## Pull requests и разрешение пользователя

- Пользователь заранее разрешил агенту принимать и сливать готовые PR ansa
  после применимых проверок и review, без отдельного ручного подтверждения.
  PR нужны для истории изменений; не останавливай работу на просьбе «проверь PR».
- Создай PR с результатами, сразу выполни доступное GitHub approval и merge
  проверенного head SHA. Незакрытые обязательные gates не объявляй PASS.
- GitHub запрещает APPROVE от автора собственного PR. При этом ограничении
  запиши результат review через COMMENT и продолжай разрешённое слияние;
  не выдавай COMMENT за formal approval и не запрашивай другой аккаунт.
- Слияние design proposal сохраняет его статус и prerequisites; само по себе
  оно не разрешает live calls, внедрение предложенных features или удаление C++.

## Инварианты

- Разделяй core, providers, tools, sessions и frontend. CLI и TUI используют общие typed commands/events с версией, ID и cursor для snapshot/resync. Provider получает описания tools, а compiled tool регистрируется без правок loop, provider или журнала.
- Один serialized control owner меняет runtime state. Блокирующие операции выполняет worker. Команды, события и completion передаются с владением данными и ограниченными буферами. Адресный `Cancel` доступен при заполненной обычной очереди. Перед dispatch сверяй incarnation и request identity. Запоздалые completion не продолжают отменённый turn.
- Исполняй tools после проверки всего успешного terminal response и полной валидации batch. Отклонённый или незавершённый response вызывает ноль executors. Requested и returned model сверяй отдельно. Runtime присваивает operation IDs.
- Сохраняй порядок и полные значения provider-native items, encrypted reasoning, call/result IDs и точный context epoch. Replay тестирует production builder и не меняет provider, model или account неявно.
- Отмена прекращает дальнейший dispatch. Она не отменяет уже совершённый эффект. Сохраняй известный результат. Неясный эффект обозначай `effect_unknown` и не повторяй операцию автоматически. До side effects нужны durable intent, recovery и проверка конфликтов записи без обещаний atomic CAS.
- Веди один canonical append-only session journal. Snapshot frontend и производные индексы не становятся вторым источником истины. Реальные auth credentials держи вне исходников, fixtures, логов и журнала. Не открывай `.env`, `.env.*` и credential stores существующих агентов.
- Full access не является sandbox. no_new_privs и ограничения capabilities не отменяют уже совершённые или remote effects; при неизвестном cleanup сохраняй unknown. Новые библиотеки и подсистемы требуют конкретной необходимости.

## Откат к C++

Исходник C++ сохранён в tag `migration/m9-cutover`, проверенный executable и Nix GC root - в `~/Workspace/pet/ansa-baselines/m9-742d29c/`. Текущая сборка и обязательные Rust gates не требуют C++ oracle. Сохраняй актуальные совместимые state; не восстанавливай старые credentials или journal после новых effects. Исторические differential captures запускаются из pre-M10 checkout.

## Migration Rust

1. Работай в отдельной ветке `migration/rust-runtime` или изолированном worktree. Сохраняй source SHA, C++ executable/Nix closure и проверенный baseline. Порядок: capture behavior → Rust equivalent → differential/adversarial tests → validate → replace. Выполняй scope выбранного milestone, не весь перенос одним diff.
2. Начальная граница — `rust/` Cargo workspace с `ansa` и `ansa-linux`; module boundaries определяет план. Edition 2024, compiler/MSRV из pinned Nix, согласованные manifest/toolchain и committed Cargo.lock. Core использует owned values, typed IDs и закрытые admission/durable receipt constructors; не вводи framework или crate на каждый module без необходимости.
3. Один owner владеет mutable runtime state. Tasks/jobs/results имеют явного владельца и join lifecycle; каналы bounded. Blocking journal, file/search/AGENTS и auth I/O не находятся в owner; медленное чтение tool не занимает единственный journal writer. Cancel независим от Submit/backpressure, прекращает будущий dispatch и сохраняет late known outcome. Drop/abort future не означает process cleanup, durable outcome или rollback side effect. Общий Arc<Mutex<Everything>> не используется.
4. В `ansa` запрещён собственный unsafe. В `ansa-linux` сначала safe std/rustix APIs; каждый необходимый unsafe block требует узкой границы, safety argument, native adversarial tests и independent review. Особенно проверяй fork/pre_exec, signals, FD ownership и supervisor lifecycle. Language memory safety не заменяет admission, permissions или durable ordering.
5. Сохраняй legacy v1 journal/auth/process-evidence codecs, identities, exact epochs и provider provenance. Различай native JSON values/number lexemes и C++ dump bytes, участвующие в hashes: Value/RawValue сами по себе parity не доказывают. Сверяй production builders и оба направления C++↔Rust read/append/reopen. Unknown/unsupported state не исправляется overwrite или автоматическим replay.
6. До разрешённого live pilot используй synthetic stores/fixtures. Не открывай реальные credentials ради planning/inspection. После rotating refresh запрещён rollback старого credential backup; после side effects нельзя откатывать journal к старой копии и автоматически продолжать. Fallback C++ читает наиболее свежий совместимый state; иначе остановка dispatch и явное восстановление.
7. Rust gate: locked build/test, fmt, clippy, native aarch64, meaningful differential/fault tests и контроль зависимостей/ресурсов по milestone. Пустой, skipped либо неподдерживаемый required suite не считается PASS. Исторические C++ comparisons выполняй отдельно на frozen reference; они не входят в текущий Rust-only gate. Сравнивай одинаковые build profiles. Исправление oracle ошибки требует отдельного safety assertion, не ослабления проверки.
8. C++ удаляется только после полного offline/live parity, двунаправленной совместимости, проверенного rollback, independent review и разрешения на cutover/удаление. Handoff различает C++ last known good, Rust last verified SHA, WIP и незакрытые gates. Ни docs-only PR, ни наличие Rust UI не меняют production language.

## Quality gate перед закрытием milestone

1. Собери production Rust entry point, полный locked Cargo workspace test, fmt/clippy, Nix package/check, native adversarial/PTY, replay mutants и pinned ASan. Команды и обязательный Rust-only inventory в [README](README.md) и [M10](docs/migration/M10.md). Unsupported sanitizer остаётся открытым gate, а не PASS. Зафиксируй платформу, конфигурацию, точное число тестов и исход проверки; docs-only review не закрывает runtime milestone.
2. Проверь `git diff` и `git diff --check`. Попробуй опровергнуть реализацию. Ищи непокрытые отказы, races, ошибки владения ресурсами, потерю состояния, нарушение лимитов и связанность слоёв. Happy path сам по себе gate не закрывает.
3. Исправь дефекты, добавь регрессии и повтори затронутые проверки. Независимый review кода проводи до полной матрицы по рабочему циклу выше. Новый finding после checkpoint требует исправления и адресного повторного review. Запиши, какой review состоялся; self-review не заменяет независимый.
4. Сделай Git checkpoint. Обнови [docs/PROJECT_STATE.md](docs/PROJECT_STATE.md) со ссылкой на проверенный коммит, результатами, непроверенными gates и следующим шагом. Черновой checkpoint помечай отдельно от last known good.
