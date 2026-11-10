# Состояние oh-my-harness

Обновлено 2026-11-11. Milestone **offline AgentRuntime закрыт**. Last known good для кода - `10a11dd` на `main`. Протокольный baseline - `ede828d`. [Отчёт о проверках](implementation-report.md#offline-agentruntime).

## Что работает

| Компонент | Поведение |
|---|---|
| CMake, pinned Nix flake, `omh_core`, `omh` | Native ELF64 AArch64. GCC 16.2.0, libcurl 8.22.0, yyjson 0.12.0, OpenSSL 3.5.8. |
| SSE/JSON, HTTP gate, Responses admission | Incremental framing, UTF-8, EOF, MIME, лимиты и полная валидация terminal response. Requested и returned model проверяются отдельно. Failed, incomplete, malformed и отклонённые batch дают ноль executor invocations. |
| Registry и native replay | Единственный production tool - pure `get_test_value`. Production continuation сохраняет prefix, ordered native items, opaque values, call IDs и exact context epoch. Обе мутации replay по-прежнему обнаруживаются. |
| Runtime | Один control owner, worker для generation/tools/replay и stop worker для синхронных stop callbacks. Адресный Cancel проходит при полном Submit FIFO, выигрывает у pending completion и останавливает дальнейший dispatch. Late completion сохраняет известные outcomes уже начатых tools. |
| Commands/events v1 | `Submit`, `Cancel`, `GetSnapshot`, typed IDs и runtime incarnation. Bounded Submit dedup. После переполнения outbox snapshot восстанавливает актуальный turn, cursor и состояние начатых tools. |
| CLI | `--self-check`, `--offline-demo` и `--offline-jsonl`. Mock → reducer → tool → production continuation → `Done`. Demo возвращает `OMH_TOOL_OK:OMH_FIXED_VALUE_42`, один executor invocation. |

Черновик `dc6766f` в `wip/offline-runtime` побайтно совпадал с исходными незакоммиченными runtime-файлами. Реализация продолжена поверх них. WIP checkpoint доступен для сравнения с исходным черновиком.

## Проверка `10a11dd`

На X13s, NixOS/aarch64, Debug CTest **20/20**, включая оба replay mutants. Release Nix package **20/20**. ASan/UBSan с leak detection и остановкой на UB **18/18** без повторной сборки mutants. Восемь runtime suites прошли по 100 запусков **800/800**. Запущены установленные `--self-check`, `--offline-demo` и JSONL handshake. PTY regression с заполненным stdout прошёл. `git diff --check` прошёл.

Независимый adversarial review проверил mailbox priority, dispatch, shutdown, stop callbacks, владение результатами и JSONL backpressure. Регрессии охватывают заблокированный provider и stop callback, Cancel перед completion и между tools, stale incarnation, дедупликацию и overflow, resync, отклонённые responses и shutdown с queued Submit. Отдельная PTY-проверка воспроизводит проблему синхронизации iostream со stdio. Contract suite проверяет отклонение valueless variant на pinned GCC.

Команды сборки, CTest, ASan/UBSan и PTY regression - в [README](../README.md). Протокольный baseline `ede828d` отдельно проходил CTest **11/11**, ASan/UBSan **9/9**.

## Ограничения и следующий шаг

- Runtime держит одну сессию, один turn за раз и один tool batch на turn. Snapshot, Submit IDs и результаты хранятся в памяти в пределах incarnation. Provider и stop callbacks должны кооперативно завершаться для shutdown. [Архитектура](architecture.md#offline-agentruntime).
- ThreadSanitizer, длительные сессии и x86_64-linux пока открыты. Live HTTPS/TLS, фактическое поведение C++ adapter на SIWC и live encrypted replay также остаются отдельными проверками. P0 live evidence относится к предыдущим probes.
- Следующий milestone - canonical append-only journal, durable intents и проверка crash recovery. Side-effecting tools требуют effect reconciliation и проверки конфликтов записи.
- OAuth/OIDC registration, PKCE/state/nonce, JWKS validation, identity binding, secret storage, concurrent refresh, file tools, ProcessRunner, TUI и subagents остаются отдельными этапами.

## Первоисточники

Начинай с `src/`, `include/omh/`, `tests/`, [README](../README.md), [architecture.md](architecture.md) и [implementation-report.md](implementation-report.md). Внешние справочные материалы на X13s читай для конкретного вопроса.

| Источник в `/home/gusevsaint/harness-research/` | Что искать |
|---|---|
| `20261110T194330Z/{00-executive-summary,05-auth-and-api-feasibility,06-personalized-requirements}.md` | Исходная постановка и границы SIWC. |
| `20261110T194330Z/{07-system-design,08-architecture-decisions,09-implementation-roadmap,10-evaluation-plan,sources}.md` | Дизайн, решения, очередность, оценка и ссылки на первоисточники. |
| `20261110T202238Z-siwc-probe/result.md` | Результат предыдущей live проверки SIWC. |
| `20261110T203326Z-p0-function/{p0-report.md,protocol_probe.py,test_protocol.py,review/review.md,review/closure.md}` | Function-call P0, исходные adversarial cases и замечания независимого review. |

Используй reports, source и synthetic fixtures. Credential stores, токены и transcripts не входят в handoff.
