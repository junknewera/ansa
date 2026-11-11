# Состояние oh-my-harness

Обновлено 2026-11-11. Milestone **persistent offline sessions, recovery и compact закрыт**. Last known good для кода - `c0295bd` на `main`. Предыдущие baselines - offline runtime `10a11dd` и protocol `ede828d`. [Отчёт о проверках](implementation-report.md#persistent-sessions-recovery-и-compact).

## Что работает

| Компонент | Поведение |
|---|---|
| Native runtime | C++20, pinned Nix flake, ELF64 AArch64 на X13s. GCC 16.2.0, libcurl 8.22.0, yyjson 0.12.0, OpenSSL 3.5.8. Один control owner, bounded mailboxes, отдельные generation/tools, storage и stop workers. |
| Protocol и tools | Строгие SSE/JSON/HTTP/Responses admission и batch validation, pure `get_test_value`, production native continuation. Оба replay mutants обнаруживаются. |
| Canonical journal | Exclusive session lock, append-only framed records и hash chains. CommandAccepted, intent, outcome и Done проходят fsync barriers. Синхронизируются также parent/session directories. |
| Recovery | Явный Resume после restart. Сохранённый prefix tools использует прежние operation IDs, исполненные calls не повторяются. Intent без outcome даёт effect_unknown и блокирует Resume. |
| History и compact | Новый turn получает контекст завершённых turns. Offline compact запускается по JSON/item budget. Native compact output и exact epoch сохраняются до generation. Полная история остаётся в журнале. |
| Ошибки хранения | Snapshot и Cancel доступны при блокированном storage worker. При write/sync failure dispatch прекращается, известный outcome остаётся в публичном snapshot с durable:false. Оборванный хвост сохраняется, новые записи идут в связанную part. Повреждение середины блокирует запись. |
| Frontend v1 | Submit, Cancel, GetSnapshot, Resume, typed IDs и incarnation. Durable Submit dedup между restart, payload conflicts, CommandReplayed, bounded outbox со snapshot/resync. |
| CLI | Self-check, offline demo и JSONL. `omh --offline-jsonl --session .omh-session` создаёт или открывает сохранённую сессию. Два запуска CLI с одним журналом проверены. |

Черновик `dc6766f` в `wip/offline-runtime` сохранён. Проверенные commits и последующие изменения доступны в Git.

## Проверка `c0295bd`

На X13s, NixOS/aarch64, Debug CTest **37/37**, включая оба replay mutants. Release Nix package **37/37**. ASan/UBSan с leak detection и остановкой на UB **35/35** без сборок mutants. Пять новых concurrency suites прошли по 20 повторов **100/100**. PTY regression с заполненным stdout прошёл. Установленный executable прошёл self-check, offline demo и persistent JSONL handshake.

Crash tests посылают SIGKILL собственным дочерним процессам после fsync acceptance, admitted response, intent, outcome, continuation и Done. Дополнительно проверены crash после compact и между двумя tools batch. Generation и executor не обходят durability barriers. Дубликаты Submit/Resume не создают повторное выполнение.

Независимый adversarial review проверил journal/replay, control owner, checkpoints, cancellation, shutdown и immutable state ownership. Все findings исправлены и закреплены регрессиями. В их числе known outcome при failed outcome/cancel append, fsync parent directory, compact item reserve, Resume replay, part-count limit и отсутствие ложных tool events до dispatch. `strace` подтвердил порядок fsync parent, part и session directory. `git diff --check` прошёл.

Команды воспроизведения - в [README](../README.md). Ранее `10a11dd` проходил CTest **20/20**, ASan/UBSan **18/18**. Protocol baseline `ede828d` проходил **11/11** и **9/9** соответственно.

## Ограничения и следующий шаг

- Production использует offline mock и единственный pure tool. Mock compact возвращает synthetic opaque state и retained user message. Качество semantic compaction, token budget и доступность native compact для SIWC остаются live gates. [Контракт и границы](architecture.md#compact).
- SIGKILL и fault injection проверены. Физическое отключение питания, поведение накопителя при power loss, ThreadSanitizer, x86_64-linux и длительные сессии пока открыты. Durability опирается на успешный fsync файловой системы и оборудования.
- Session journal version 1 восстанавливается полным replay. Миграции, отдельные индексы и ротация по общему размеру отложены. По умолчанию лимит Submit IDs - 4096, records - 32 MiB, parts - 4096. Compact уменьшает активный контекст; история журнала сохраняется.
- Reconciliation неизвестных эффектов требует отдельного workflow. Side-effecting tools и file replacement semantics остаются отдельными gates. Advisory lock и проверки inode/size ограничены взаимодействием с внешними writers.
- Следующий шаг - отдельный milestone собственного OAuth/SIWC provider с live admission/replay и проверкой compact. Own registration, PKCE/state/nonce, JWKS, identity binding, secure storage и concurrent refresh должны иметь собственные тесты. File tools, ProcessRunner, TUI и subagents остаются дальнейшими этапами.

## Первоисточники

Начинай с `src/`, `include/omh/`, `tests/`, [README](../README.md), [architecture.md](architecture.md) и [implementation-report.md](implementation-report.md). Внешние справочные материалы на X13s читай для конкретного вопроса.

| Источник в `/home/gusevsaint/harness-research/` | Что искать |
|---|---|
| `20261110T194330Z/{00-executive-summary,05-auth-and-api-feasibility,06-personalized-requirements}.md` | Исходная постановка и границы SIWC. |
| `20261110T194330Z/{07-system-design,08-architecture-decisions,09-implementation-roadmap,10-evaluation-plan,sources}.md` | Дизайн, решения, очередность, оценка и ссылки на первоисточники. |
| `20261110T202238Z-siwc-probe/result.md` | Результат предыдущей live проверки SIWC. |
| `20261110T203326Z-p0-function/{p0-report.md,protocol_probe.py,test_protocol.py,review/review.md,review/closure.md}` | Function-call P0, исходные adversarial cases и замечания независимого review. |

Используй reports, source и synthetic fixtures. Credential stores, токены и transcripts не входят в handoff.
