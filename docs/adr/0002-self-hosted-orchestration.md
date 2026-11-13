# ADR 0002 — Self-hosted Orchestrate с управлением в ansa

Дата: 2026-11-13. Статус: **Proposed**. Scope: архитектура и будущий план, без implementation/live authorization.

Основания, current audit, официальные источники, typed contracts, fault cases и eval plan: [ORCHESTRATION_RESEARCH.md](../ORCHESTRATION_RESEARCH.md). Future implementation/review prompts: [ORCHESTRATION_PROMPTS.md](../ORCHESTRATION_PROMPTS.md).

## Context

Проверенный main `ca851c99d4887a121300f0bcee8687ed9d055071` содержит production C++20 и merged Rust migration plan из PR #1. PR #2 с head `c06b1a839695e0690ead1db88c5e2a2df9a198db` содержит M0 scaffold/harness, но остаётся open; native X13s acceptance — NO-GO. Rust runtime ещё не существует. Эти facts относятся к snapshot, не являются обещанием будущего состояния веток.

ANSA уже владеет native agent loop, bounded owner/mailboxes, canonical session journal, admission и process evidence. Отсутствует workflow goal → implementation → verification → independent review → integration. Сегодня человек переносит задания и результаты между reasoning session и coding agent.

Нужна автоматизация этого workflow без ChatGPT Pro, GUI automation, неофициальных endpoints, microservices и unbounded agent trees. Rust migration M0–M10 имеет собственные compatibility/cutover gates; новый слой не должен усложнять их или переноситься дважды.

## Decision

### 1. Hybrid, собственный control plane

Orchestrate — небольшой deterministic module будущего Rust ansa. Native read-only sessions выполняют planning/research и independent review. Основной coding backend — официальный Codex app-server по локальному stdio. Codex владеет только bounded execution attempt. Task lifecycle, budgets, worktrees, retry/cancel, acceptance и integration остаются у ansa.

Codex — опциональная executor dependency режима Orchestrate, а не dependency basic/plan или core agent loop. Поздний NativeAnsaExecutor реализует тот же контракт только при положительных evals. Не создавать два реальных executor adapters сразу: `codex exec --json` остаётся возможной более узкой альтернативой/spike.

### 2. SIWC primary inference, API optional

Использовать собственный SIWC grant ansa и официальный разрешённый Responses профиль. Собственный access token может передаваться app-server по документированному provider configuration; чужие credential stores не читаются. API-key inference — явный отдельный route с собственным бюджетом, без silent fallback.

ChatGPT subscription, SIWC plan usage, signed-in Codex usage и API billing — разные accounting boundaries. Текущий account-specific catalog берётся через разрешённый route; app-server catalog не считается entitlement proof. Live availability остаётся deployment gate. Ни один runtime component не требует Pro session или контекста этого исследования.

### 3. Минимальные роли и ограничения

MVP: coordinator, один coding worker, один fresh reviewer и обычный verification runner. Researcher/recovery analysis объединены с planner. Integrator и scheduler — код.

Один write worker, один root run на repository common directory, internal Codex subagents выключены. Начальная policy: initial attempt + не более двух repairs, одна replan, bounded provider retries и total root budget. Restart/new session не сбрасывают counters. Параллельные два builders — отдельный следующий этап с eval.

### 4. Один task authority

Создать отдельный versioned canonical orchestration journal namespace, используя будущие Rust framing/fsync/storage primitives. Не добавлять orchestration events в strict legacy session v1 и не вводить SQLite/второй session store в M4.

Task/attempt/budget/approval/integration state меняет только orchestration owner. Native session journals сохраняют authority исключительно для собственной model/tool history. Immutable supervisor/test/Codex evidence импортируется root по IDs и digest; само evidence не меняет task state. Markdown, UI snapshots и индексы — derived.

Новая schema не обязана читаться старым C++; старый binary не продолжает новые orchestration runs. Rollback останавливает feature и сохраняет freshest evidence/state, не восстанавливает старый journal/credentials после effects.

### 5. Две durable launch boundaries

`TaskId`, `AttemptId`, `OwnerEpoch`, source SHA, policy и specification digest фиксируются до effect. Supervisor consume-once marker защищает process launch. Отдельный durable `turn/start` intent защищает protocol dispatch; JSON-RPC request ID не считается idempotency key.

После restart старый receipt сверяется с сохранённой epoch его attempt и импортируется текущим owner отдельным transition; старые completion не dispatch-ят работу напрямую. Lost ACK, no terminal, parent/supervisor crash → reconciliation. Запрещён blind resend неизвестного turn. Продолжение возможно только после подтверждённого quiescence, file inventory и классификации effects. Exactly-once произвольного shell/remote API не обещается.

### 6. Cancellation и recovery

Разделять protocol terminal, process termination, cleanup, effect knowledge и acceptance. `completed` не означает PASS; `interrupted` не означает rollback; process exit 0 не означает completion. Cancel немедленно запрещает будущий dispatch, но known late outcome сохраняется.

Стратегия MVP — stop-and-reconcile. Не обещать reconnect к прежним stdio pipes или uninterrupted child execution после parent crash. Для unattended deployment требуется проверенный supervisor/cgroup lifecycle; если local cleanup не подтверждён, задача BlockedCleanup, repository/worktree остаются quarantined, в том числе для нового root run. Cancelled terminal допускается после local quiescence; external unknowns остаются явными. Systemd/cgroup profile создаётся после migration, не расширяет M6 parity.

App-server command всё ещё отмечен experimental в официальной CLI reference. Поэтому нужны pinned version/binary/schema, stable API subset и offline conformance/fault suite. Server-initiated approval/user-input requests получают deterministic schema-valid denial либо bounded pause, никогда silent auto-accept/hang. WebSocket и daemon API не требуются. Thread resume использует сохранённый controlled Codex state directory исходной lineage; новая пустая per-attempt directory не содержит её history. Одновременно lineage принадлежит одному process.

### 7. Git и независимая acceptance

Каждый writer получает отдельный worktree от immutable base. После остановки worker host проверяет полный diff/inventory и scope; unexpected writes не скрываются. Verification manifest заморожен до builder; host запускает required tests на frozen candidate и хранит actual commands/status/counts/log hashes.

Fresh reviewer получает original requirements, actual diff/source и trusted evidence через read-only inputs. Builder summary не является единственным источником. Одна модель в двух contexts допустима, но ошибки могут коррелировать; отдельная модель не заменяет fault tests.

Integrator последовательно собирает собственную branch, повторяет gates на итоговом SHA и публикует draft PR только в пределах policy. Accepted deliverable не равен merged main. Lost push/PR ACK требует reconciliation по exact remote head/PR marker. Default main/release/force-push запрещены. Non-Git Orchestrate MVP возвращает blocker, basic/plan не меняются.

### 8. Полномочия и credentials

Full access ansa остаётся сознательной политикой владельца. Для builder рекомендуются заранее разрешённый workspace-write profile, isolated state, filtered environment и mediated publication. Не спрашивать каждую обычную test command; опасные действия требуют action-bound policy.

Модель не меняет policy, budgets или acceptance requirements. Login/logout, новые credential grants, destructive data actions, security changes, несовместимый storage upgrade и новые paid services требуют отдельного разрешения, если оно не было дано заранее для этого действия.

Worktree, hashes, prompt и no_new_privs не sandbox. Same-UID full-access writer способен нарушить логические границы, читать secrets или менять evidence. Hard security требует реально enforced sandbox/OS authority; cgroup не решает authentication. Не удерживать native ToolLease на весь Codex run; token renewal — отдельный serialized auth lifecycle, без refresh lock cycle и без model доступа к tokens.

## Sequencing

**Default production implementation после verified M10 GO**, отдельным Orchestrate feature milestone. Это решение о порядке работ, не техническое утверждение, что удаление C++ требуется для orchestration. M0–M10 scope остаётся неизменным.

До этого: O0 docs/contracts сейчас; после verified M6 возможен узкий non-production lifecycle spike. Вариант implementation после M8/M9 до M10 допустим только отдельным решением, если cutover стабилен, а M10 задержан организационно; такой случай не подразумевается этим ADR.

| Этап | Результат |
|---|---|
| O1 | Offline task journal/reducer, repository lock, fake executor и scripted verification/review receipts, crash/cancel/budget tests. |
| O2 | Pinned Codex adapter, owned worktree, supervised lifecycle, secure token/environment boundary. |
| O3 | Host verification, fresh review, integration, recoverable publication. |
| O4 | Отдельно разрешённый X13s live end-to-end pilot и comparison eval. |

## Consequences

Плюсы: собственный управляющий runtime; reuse существующих сильных contracts; Codex решает coding, без переноса его internals; минимум новых процессов и storage; замена executor не переписывает acceptance; handoff не требует старого чата.

Цена: app-server compatibility surface, extra inference для planning/review, evidence artifacts, Git lifecycle и честные blocked outcomes при неопределённости. Успех orchestration ещё не измерен. Отсутствие доказанного выигрыша в accepted tasks/human interventions — допустимое основание оставить single-agent path.

Non-goals: swarms, unbounded recursion, self-modifying skills, framework/plugin platform, database services, hidden Pro dependency, browser-driven inference, automatic main merge, full-access sandbox claims и transactional rollback внешних effects.

## Acceptance этого решения

Architecture review должен подтвердить disjoint journal authority, two-phase effect handling, route legality, bounded budgets и отсутствие migration scope creep. Реальная functionality принимается только этапами O1–O4. Принятие docs не закрывает M0, не утверждает availability конкретной модели и не разрешает paid inference.
