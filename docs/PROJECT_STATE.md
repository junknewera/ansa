# Состояние oh-my-harness

Обновлено 2026-11-11. Текущий milestone - **offline AgentRuntime в работе**. Протокольный milestone закрыт. Last known good для кода - `ede828d` на `main`. Результаты ниже относятся к этому коду. [Отчёт о протокольном milestone](implementation-report.md).

## Что работает и проверено

| Компонент | Проверенное поведение |
|---|---|
| CMake, pinned Nix flake, `omh_core`, `omh` | Native build и запуск на X13s, NixOS/aarch64. GCC 16.2.0, libcurl 8.22.0, yyjson 0.12.0, OpenSSL 3.5.8. |
| SSE/JSON и HTTP gate | Incremental framing, UTF-8 и EOF, лимиты, duplicate keys, строгий MIME. Missing MIME разрешается только явной политикой для точного SIWC Responses endpoint. |
| Responses reducer и admission | Terminal state, сверка model/item/call IDs и arguments. 87 отклонённых reducer fixtures, 13 HTTP/MIME, 2 batch/schema. При отказах ноль executor invocations. |
| Registry и native replay | Pure `get_test_value`, owning snapshot и проверка batch, отдельные operation IDs. Production replay сохраняет prefix, native items, opaque values, call IDs и exact epoch. Оба mutation-теста ловят порчу replay. |
| CLI | `--self-check` и синтетический `--offline-demo`. Demo возвращает `OMH_TOOL_OK:OMH_FIXED_VALUE_42` и один вызов инструмента. |
| Frontend contract | На `ede828d` проверены типы v1 и базовая валидация. Выполняемый runtime и JSONL frontend относятся к текущему milestone. |

## Как воспроизвести проверенный baseline

В чистом checkout `ede828d` команды ниже дают полный CTest **11/11**, включая две мутации. Nix package проходит тот же набор. `ctest --no-tests=error` на пустом каталоге возвращал 8. ASan/UBSan с отключённой пересборкой mutants прошёл **9/9**.

Оба набора повторно пройдены на X13s из отдельного checkout при подготовке handoff. Агент без истории диалога по файлам и Git восстановил baseline, состояние черновика, первый шаг и критерии приёмки. Независимый review относился к документам.

```sh
nix develop --no-update-lock-file -c sh -c '
  cmake -S . -B build -G Ninja -DCMAKE_BUILD_TYPE=Debug &&
  cmake --build build -j 4 &&
  ctest --test-dir build --output-on-failure --no-tests=error
'
nix build --no-update-lock-file
./result/bin/omh --self-check
./result/bin/omh --offline-demo
nix develop --no-update-lock-file -c sh -c '
  cmake -S . -B build-sanitize -G Ninja -DCMAKE_BUILD_TYPE=Debug -DOMH_SANITIZERS=ON -DOMH_MUTATION_CHILD=ON &&
  cmake --build build-sanitize -j 2 &&
  ctest --test-dir build-sanitize --output-on-failure --no-tests=error
'
```

## Незавершённый runtime и точка продолжения

Черновик сохранён локальной веткой `wip/offline-runtime`, checkpoint `dc6766f` от `ede828d`. В исходном рабочем дереве лежат те же изменения, часть файлов отображается как `??`. Сверяй `git status --short`, tracked diff и новые файлы с `git show --stat dc6766f`. Поверх присутствующего черновика checkpoint не применяй. В чистом checkout с локальной веткой можно перенести его на рабочую ветку от `main` командой `git cherry-pick --no-commit dc6766f`. Без локального checkpoint критериев ниже достаточно для реализации с `main`.

В черновике есть `src/runtime.cpp`, `src/offline.cpp`, `src/cli.cpp` и заголовки `include/omh/{runtime,offline}.hpp`. Намечены владелец состояния, worker, отдельный слот отмены, bounded outbox, mock через production decoder и JSONL adapter. Изменены контракты и callback `PreparedBatch::execute`. Черновик не собран и не проверен. Он не считается реализацией milestone.

Первый шаг - привести черновик к единому контракту. В заголовке уже появились `CommandId`, runtime incarnation и `GetSnapshot`, но реализация, CLI и тест `contracts` ещё используют старые типы. `Runtime::incarnation()` объявлен без определения. CMake и `main.cpp` ещё не подключают runtime и frontend. Dedup, проверки incarnation и тесты гонок предстоит реализовать.

При review черновика проверь dispatch во время shutdown. Worker может увидеть queued job после `aborting_`, пока owner ещё не остановился. Проверь также блокировку `Cancel` при медленном stdout. Стандартная привязка `std::cin` к `std::cout` может задержать чтение команды. События начала и завершения tool должны соответствовать реальному dispatch и outcome.

## Scope и acceptance offline AgentRuntime

- Один control owner принимает команды из bounded thread-safe queue. Отдельный worker выполняет mock generation. Provider можно детерминированно заблокировать, завершить, отменить и заставить вернуть late success. Для передачи результата переиспользуй strict Responses reducer, compiled ToolRegistry и production continuation.
- Versioned commands `Submit`, `Cancel`, `GetSnapshot`. Typed events `CommandAccepted`, `TextDelta`, `ToolStarted`, `ToolFinished`, `Cancelled`, `Done`, `RuntimeError` (wire `error`), `FrontendSnapshot`, `ResyncRequired`. Используй `SessionId`, `TurnId`, `RequestId`, `CommandId` и runtime incarnation. JSONL CLI принимает `submit`, `cancel`, `snapshot`. Начальный snapshot сообщает incarnation, а `CommandAccepted` и активный snapshot дают `RequestId` для адресного Cancel.
- Cancel при заблокированной генерации обрабатывается владельцем состояния. Детерминированно проверь приоритет, когда matching Cancel и successful completion одновременно ожидают обработки. Late completion после Cancel вызывает ноль executors. Stale Cancel из прежней incarnation не влияет на новый запрос.
- Повторный Submit с тем же `CommandId` не создаёт второй turn в пределах incarnation. Храни IDs с лимитом, а при исчерпании явно отклоняй новые IDs. Overflow обычной очереди виден клиенту, а адресный Cancel проходит. Пропуск событий обнаруживается, in-memory snapshot восстанавливает актуальный state и cursor.
- Failed, incomplete и malformed provider responses вызывают ноль executors. Успешный turn проходит mock → reducer → pure `get_test_value` → production continuation → `Done`. Набор протокольных тестов продолжает проходить.
- Закрой gate нативной сборкой NixOS/aarch64, полным CTest, ASan/UBSan и review по [AGENTS.md](../AGENTS.md). Сделай отдельный checkpoint кода. Запиши его hash, точное число тестов и ограничения в отчёте и этом handoff.

Текущий scope ограничен pure `get_test_value`. Persistent OAuth, session journal, ProcessRunner, read/write/edit/bash, TUI и subagents отложены. Следующий milestone после приёмки runtime - canonical append-only journal, durable intents и проверка восстановления после сбоя. Frontend snapshot обслуживает in-memory resync. [Архитектурные границы](architecture.md#следующая-граница).

## Решения, ограничения и открытые проверки

- Agent loop независим от Codex. Provider, tools, sessions и frontend имеют отдельные границы. Provider передаёт только проверенные native items и tool calls. Один control owner решает dispatch и recovery. Durable storage появится как один источник истины. [Архитектура и инварианты](architecture.md).
- Адаптер поддерживает закрытое подмножество Responses. Live HTTPS/TLS, фактическое поведение C++ adapter на SIWC и live encrypted replay не проверялись. P0 live evidence относится к предыдущим probes.
- У `ede828d` есть результаты сборки и тестов. Независимый code review этого коммита в репозитории не зафиксирован. Review P0 относится к предыдущим probes.
- OAuth/OIDC registration, PKCE/state/nonce, JWKS validation, identity binding, secret storage и concurrent refresh rotation остаются отдельными gates.
- Для runtime открыты cancellation races, bounded dedup и recovery публичного состояния. После него остаются crash durability, effect reconciliation и file replacement semantics. ASan/UBSan не проверяет data races сам по себе. x86_64-linux и длительные сессии не проверены.

## Первоисточники

Начинай с `src/`, `include/omh/`, `tests/`, [README](../README.md), [architecture.md](architecture.md) и [implementation-report.md](implementation-report.md). Они задают воспроизводимый baseline и текущую область разработки. Внешние справочные материалы на X13s читаются только при необходимости.

| Источник в `/home/gusevsaint/harness-research/` | Что искать |
|---|---|
| `20261110T194330Z/{00-executive-summary,05-auth-and-api-feasibility,06-personalized-requirements}.md` | Исходная постановка и границы SIWC. |
| `20261110T194330Z/{07-system-design,08-architecture-decisions,09-implementation-roadmap,10-evaluation-plan,sources}.md` | Дизайн, принятые решения, очередность, оценка и ссылки на первоисточники. |
| `20261110T202238Z-siwc-probe/result.md` | Результат предыдущей live проверки SIWC. |
| `20261110T203326Z-p0-function/{p0-report.md,protocol_probe.py,test_protocol.py,review/review.md,review/closure.md}` | Function-call P0, исходные adversarial cases и замечания независимого review. |

Используй reports, source и synthetic fixtures. Credential stores, токены и transcripts не входят в handoff.
