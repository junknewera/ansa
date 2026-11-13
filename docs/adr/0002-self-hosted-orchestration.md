# ADR 0002 — Native ANSA Orchestrate через direct SIWC Responses

Дата: 2026-11-13. Статус product decision: **ACCEPTED — обязательное требование пользователя**. Детали implementation contracts и roadmap: **PROPOSED**. Scope: архитектура и будущий план, без implementation/live authorization.

DECISION: Native ANSA multi-agent orchestration via direct SIWC Responses. No Codex runtime dependency.

Основания, current audit, официальные источники, typed contracts, fault cases и eval plan: [ORCHESTRATION_RESEARCH.md](../ORCHESTRATION_RESEARCH.md). Future implementation/review prompts: [ORCHESTRATION_PROMPTS.md](../ORCHESTRATION_PROMPTS.md).

## Context

Проверенный main `ca851c99d4887a121300f0bcee8687ed9d055071` содержит production C++20 и merged Rust migration plan из PR #1. PR #2 с head `c06b1a839695e0690ead1db88c5e2a2df9a198db` содержит M0 scaffold/harness, но остаётся open; native X13s acceptance — NO-GO. Rust runtime ещё не существует. Эти facts относятся к snapshot, не являются обещанием будущего состояния веток.

ANSA уже владеет native agent loop, bounded owner/mailboxes, canonical session journal, admission и process evidence. Отсутствует workflow goal → implementation → verification → independent review → integration. Нужен небольшой task control plane поверх собственного loop после его Rust migration.

Выбран только собственный direct SIWC route. Codex runtime integration и paid API alternate отклонены для этого продукта. Codex может использоваться человеком как внешний инструмент разработки ansa; это не компонент запускаемого Orchestrate. Ни ChatGPT Pro, ни GUI automation, ни контекст этого исследования не нужны runtime.

## Decision

### 1. Композиция native AgentRuntime

Orchestrate — deterministic module будущего Rust ansa. Parent владеет task graph, budgets, worktrees, retry/cancel, verification, acceptance и integration. Он запускает bounded child sessions на том же native `AgentRuntime`, который обслуживает basic/plan, без второго model → tools → continuation engine.

Каждый child имеет отдельные `SessionId`, journal, context epoch, model history и валидированный `ToolSnapshot`. Planner/researcher и fresh reviewer получают read-only profile; builder — явно разрешённые native write tools в owned worktree. Разные роли не разделяют mutable conversation state. Смена роли или evidence context не выполняется скрытой подменой epoch старой session.

Child owner единолично изменяет своё session state и сохраняет admitted model/tool outcomes. Parent reducer единолично изменяет task state. Parent не редактирует child journal, child не выставляет task acceptance и не выдаёт себе право создавать другие children. Общий auth state принадлежит одному центральному broker; детям не копируются credential stores или bearer tokens в prompts/artifacts.

Композиционный wrapper над `AgentRuntime`, запрос durable child receipt и новые orchestration commands — **PROPOSED post-migration capabilities**, а не существующие API текущего scaffold. Нельзя добавлять отдельный general agent framework или заново реализовывать native protocol, journal и ProcessRunner ради child sessions.

### 2. Direct SIWC и central auth broker

Все model calls выполняет собственный SIWC Responses adapter ansa под собственным разрешённым grant. Поддержанные модели и capabilities проверяются через реальный SIWC route/account, а не static model list. Реальные account entitlement, quota и качество выбранных моделей остаются deployment gates.

HTTP contract сохраняет `store:false`, `stream:true` и explicit history; серверный `multi_agent` не требуется. Собственный native loop переносит provider-native items, encrypted reasoning, call/result IDs и exact context epoch. Полный successful terminal, EOF/transport admission и validation всего tool batch предшествуют исполнению tools. Initial configuration/epoch immutable; текущая epoch после compaction восстанавливается native reducer отдельно. Partial response/preview не становится canonical outcome.

Центральный `AuthManager`/broker сериализует refresh и auth persistence, сохраняет account/workspace binding и выдаёт разрешение на конкретный native dispatch. Сохраняются существующие operation-scoped auth lease semantics; parent не удерживает один lease на весь child lifecycle. Logout/revocation прекращает новые dispatch всех связанных children и передаёт native stop уже выполняемым operations. Await child result не должен удерживать auth lock или journal worker.

SIWC plan usage не переводится в API-dollar cost. Root учитывает все native planning/build/review/summary requests, каждый Resume и HTTP retry, доступную token usage, throttling и общий budget через restart и смену child session; число Submit не подменяет расход внутренних rounds. Недоступная telemetry остаётся unknown, не нулём. Недоступный SIWC route возвращает named blocker; paid fallback, другой account или неявная модель не подставляются.

### 3. Минимальные роли и ограничения

MVP: один coordinator, один native builder, отдельная fresh native reviewer session и обычный VerificationRunner. Researcher и bounded failure analysis объединены с planner. Integrator, recovery reconciliation и scheduler — код.

В MVP native role sessions выполняются последовательно: один active model child и один write worker, один root run на repository common directory. Произвольная recursive delegation недоступна children. Это сохраняет operation-scoped auth lock/lease semantics; одновременная работа нескольких sessions не обещается до отдельной проверки broker contention/cancellation. Начальная policy: initial attempt + не более двух repairs, одна replan, bounded provider retries и total root budget. Restart/new session не сбрасывает counters. Параллельные два builders — отдельный этап только после eval.

### 4. Один task authority и явная связь с native session

Создать отдельный versioned canonical orchestration journal namespace, используя будущие Rust framing/fsync/storage primitives. Не добавлять task events в strict legacy session v1 и не вводить второй canonical session store в M4.

До root `ChildSubmitIntent` нужно durably создать child directory, session init и binding; parent заранее знает `RunId/TaskId/AttemptId/OwnerEpoch`, immutable source SHA, policy/spec/payload digest и `SessionId/CommandId/TurnId`. Затем parent sync-ит correlation/intent и только после этого отправляет child Submit. Child store без parent intent остаётся неисполняемым orphan для reconciliation, не основанием начать работу.

`PostStatus::accepted` означает только admission в очередь, не durable acceptance. Native `RequestId` выделяется child runtime до fsync, но становится пригодным для durable root binding только после child journal fsync и проверенного receipt; он относится к тому же accepted Submit/turn. Pending snapshot не подтверждает durable acceptance. **RequestId не переопределяется как ID каждого HTTP request**; для учёта provider rounds используются отдельные ordinal/context-epoch fields без изменения v1 semantics. Frontend `stream_sequence` — event cursor, а не journal offset и не commit receipt.

Потерянный Submit ACK требует reconciliation child journal/receipt по прежнему `CommandId` и exact payload. Dedup допустим только в том же store и при полном совпадении payload. Recovery открывает исключительно `OpenExisting`; отсутствие или повреждение store блокирует работу. Fallback `CreateNew` запрещён: создание пустого journal потеряет dedup и может повторить effect. Эти имена обозначают предлагаемые безопасные open modes, а не готовые методы текущего runtime. Точный receipt lookup/wrapper должен быть определён и проверен в O2; он не выдается за существующий публичный API.

Task/attempt/budget/approval/integration state меняет только orchestration owner. Child journals остаются authority для собственной model/tool history. Known outcome импортируется root отдельным durable transition по IDs, sequence и digest; повторный импорт не повторяет effect. После parent restart origin epoch сверяется с сохранённым attempt, а запись делает текущий owner. Старая completion не dispatch-ит работу напрямую.

Между parent и child journals нет общей транзакции: отсутствующий ACK или parent import не отменяет уже durable child outcome. Неполная либо конфликтующая correlation блокирует продолжение. Markdown, snapshots и индексы — derived. Новая task schema не обязана читаться старым C++; rollback отключает feature и сохраняет freshest state/evidence.

### 5. Native execution, cancellation и recovery

Для каждой native tool operation сохраняются existing whole-response/batch admission, auth guard и durable intent до side effect, затем known/unknown outcome и durable outcome. Их внутренний порядок и lease lifetime берутся из native contract, не переопределяются orchestration wrapper. ProcessRunner сохраняет собственные consumed marker и supervisor evidence. Task lifecycle не заменяет эти barriers и не обещает exactly-once произвольного shell.

Parent Cancel останавливает будущие child dispatch и адресно передаётся native runtime. Child прекращает новые model/tool actions, отменяет provider request и передаёт stop своему ProcessRunner; parent сохраняет job/join ownership до результата или reconciliation. Known late outcome не теряется после отмены. `Done`, exit 0, известный failure команды и принятый engineering task — разные состояния.

При parent/child crash сначала восстановить оба journal domains и process evidence, подтвердить local quiescence и inventory. `effect_unknown` не повторяется автоматически. Неподтверждённый cleanup оставляет задачу `BlockedCleanup`, repository/worktree quarantined, в том числе для нового root run. Terminal disconnect не выдаёт другому owner право начать запись; machine restart не разрешает повторять неизвестные external effects.

Все child jobs имеют retained ownership и bounded queues. Future abort не является процессным cleanup. Строгий OS containment при необходимости проверяется отдельным post-migration profile; этот ADR не добавляет cgroups или новый supervisor в M6 parity.

### 6. Git и независимая acceptance

Каждый writer получает отдельный worktree от immutable base. После остановки native builder host проверяет полный diff/inventory и scope. Verification manifest заморожен до build; обычный runner запускает required tests на frozen candidate и сохраняет actual commands/status/counts/log hashes.

Fresh native reviewer получает original requirements, actual diff/relevant source и immutable evidence через scoped read-only tools. Builder summary не является единственным источником. Reviewer может потребовать новые negative scenarios; их запускает VerificationRunner по разрешённому manifest. Одна модель в разных contexts допустима, но ошибки могут коррелировать; Pro для этого не нужна.

Integrator последовательно собирает собственную branch, повторяет gates на итоговом SHA и публикует draft PR только в пределах policy. Accepted deliverable не равен merged main. Lost push/PR ACK требует reconciliation по exact remote head/PR marker. Default main/release/force-push запрещены. Non-Git Orchestrate MVP возвращает blocker; существующие basic/plan остаются доступны.

### 7. Полномочия и credentials

Full access ansa остаётся сознательной политикой владельца. ToolSnapshot ограничивает доступные compiled tools и dispatch, но произвольный shell под тем же UID не становится sandbox от наличия worktree или prompt. Для сильных filesystem/network/credential границ нужен реально enforced OS profile. Hashes и read-only reviewer не делают evidence tamper-proof против hostile same-UID writer.

Модель не меняет policy, budgets, acceptance manifest или свой набор tools. Обычные разрешённые tests и worktree edits автономны. Login/logout, новые grants, destructive data actions, security changes, несовместимый storage upgrade и другие необратимые side effects требуют action-bound policy, если она не была выдана заранее. Access/refresh tokens остаются у native provider/auth layer; модель, shell environment и journal их не получают.

## Sequencing

**Default production implementation после verified M10 GO**, отдельным Orchestrate feature milestone. Это решение о порядке работ, не техническое утверждение, что удаление C++ необходимо для композиции agents. M0–M10 scope остаётся неизменным.

Технический фундамент даёт существующий roadmap: M2 protocol/admission, M3 auth, M4 journal/session, M5 owner, M6 process/tools, M7 commands/frontend, M8 complete offline E2E и M9 live acceptance. Внешний agent runtime в dependencies отсутствует.

Сейчас O0 docs/contracts. После verified M6 допустим узкий изолированный non-production spike native child lifecycle. Вариант после M8/M9 до M10 возможен только отдельным решением при стабилизированном cutover; этим ADR он не назначается.

| Этап | Результат |
|---|---|
| O1 | Offline task journal/reducer, repository lock, fake native child lifecycle и scripted receipts; crash/cancel/budget tests. |
| O2 | Композиция Rust AgentRuntime/basic/plan, отдельные ToolSnapshot/context/journals/worktrees, synthetic direct Responses transport; native submit correlation, lifecycle/cancel/recovery и central auth broker. |
| O3 | Frozen VerificationRunner, fresh native review, integration и recoverable publication; fake inference и synthetic Git remote. |
| O4 | Отдельно разрешённый native X13s direct SIWC end-to-end pilot, actual model discovery, comparison eval и usage/resource measurements. |

## Consequences

Плюсы: собственный task control plane и собственный execution loop; один protocol/auth/process stack; отсутствие внешнего runtime adapter; role isolation через уже принятые native contracts; handoff без старого чата.

Цена: coding capability ограничена качеством native tools/контекста; композиция sessions, durable receipt lookup, global budgets, evidence и Git lifecycle требуют новой проверки. Успех multi-agent approach ещё не измерен. Если native single-agent path даёт лучшее accepted-task/human-intervention соотношение, использовать его для соответствующих задач.

Non-goals: swarms, unbounded recursion, self-modifying skills, framework/plugin platform, database services, hidden Pro dependency, browser-driven inference, automatic main merge, full-access sandbox claims, transactional rollback внешних effects, интеграция отвергнутого внешнего coding runtime и paid inference alternate.

## Acceptance этого решения

Architecture review должен подтвердить disjoint parent/child authority, отсутствие второго native loop, durable Submit correlation, v1 RequestId semantics, bounded budgets, central auth lifecycle и отсутствие migration scope creep. Реальная functionality принимается только этапами O1–O4. Принятие docs не закрывает M0 и не подтверждает доступность конкретной модели.

UNKNOWN до проверки: composition/recovery на native X13s, child receipt wrapper, auth contention/cancellation нескольких sessions, quota/quality overhead, coding quality native builder, полноценный cleanup и устойчивость filesystem при power loss. Ни один из этих пунктов не становится TESTED от публикации ADR.
