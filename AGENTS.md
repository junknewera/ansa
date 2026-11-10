# AGENTS.md

`oh-my-harness` - минимальный C++20 agent runtime, который легко читать, менять и расширять. Namespace `omh`, executable `omh`. Agent loop принадлежит проекту и не зависит от Codex SDK или app-server.

## Начало задачи

1. Прочитай этот файл и [docs/PROJECT_STATE.md](docs/PROJECT_STATE.md). Проверь `git status --short --branch` и последние коммиты.
2. Уточни acceptance criteria по задаче и изучи относящиеся к ним исходники и тесты. [Architecture](docs/architecture.md) объясняет границы. Исследования из PROJECT_STATE нужны для конкретного вопроса и доступны только для чтения.
3. Сделай минимальный diff и сохрани чужие и незавершённые изменения. Архитектурный blocker устрани до новых функций либо запиши, почему он отложен и какую проверку блокирует.
4. Проверяй результат реальной сборкой и тестами. Молчание пользователя не подтверждает корректность. Не возобновляй фундаментальное исследование без нового вопроса.

## Инварианты

- Разделяй core, providers, tools, sessions и frontend. CLI и TUI используют общие typed commands/events с версией, ID и cursor для snapshot/resync. Provider получает описания tools, а compiled tool регистрируется без правок loop, provider или журнала.
- Один serialized control owner меняет runtime state. Блокирующие операции выполняет worker. Команды, события и completion передаются с владением данными и ограниченными буферами. Адресный `Cancel` доступен при заполненной обычной очереди. Перед dispatch сверяй incarnation и request identity. Запоздалые completion не продолжают отменённый turn.
- Исполняй tools после проверки всего успешного terminal response и полной валидации batch. Отклонённый или незавершённый response вызывает ноль executors. Requested и returned model сверяй отдельно. Runtime присваивает operation IDs.
- Сохраняй порядок и полные значения provider-native items, encrypted reasoning, call/result IDs и точный context epoch. Replay тестирует production builder и не меняет provider, model или account неявно.
- Отмена прекращает дальнейший dispatch. Она не отменяет уже совершённый эффект. Сохраняй известный результат. Неясный эффект обозначай `effect_unknown` и не повторяй операцию автоматически. До side effects нужны durable intent, recovery и проверка конфликтов записи без обещаний atomic CAS.
- При добавлении persistence веди один canonical append-only session journal. Snapshot frontend и производные индексы не становятся вторым источником истины. Реальные auth credentials держи вне исходников, fixtures, логов и журнала. Не открывай `.env`, `.env.*` и credential stores существующих агентов.
- Используй C++20, RAII, CMake и pinned Nix flake. Держи зависимости и абстракции соразмерными текущему milestone. Новые библиотеки и подсистемы требуют конкретной необходимости. Собирай и запускай на NixOS/aarch64.

## Quality gate перед закрытием milestone

1. Собери и запусти production entry point, полный CTest с `--output-on-failure --no-tests=error`, adversarial tests и ASan/UBSan. Команды в [README](README.md). Зафиксируй платформу, конфигурацию, точное число тестов и исход проверки.
2. Проверь `git diff` и `git diff --check`. Попробуй опровергнуть реализацию. Ищи непокрытые отказы, races, ошибки владения ресурсами, потерю состояния, нарушение лимитов и связанность слоёв. Happy path сам по себе gate не закрывает.
3. Исправь дефекты, добавь регрессии и повтори затронутые проверки. Если доступен независимый review-агент, передай ему diff и критерии. Запиши, какой review состоялся. Self-review не заменяет независимый.
4. Сделай Git checkpoint. Обнови [docs/PROJECT_STATE.md](docs/PROJECT_STATE.md) со ссылкой на проверенный коммит, результатами, непроверенными gates и следующим шагом. Черновой checkpoint помечай отдельно от last known good.
