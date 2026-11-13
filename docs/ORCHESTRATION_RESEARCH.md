# ANSA — native self-hosted orchestration через direct SIWC Responses

Дата исходного исследования и обязательной архитектурной коррекции: **13 ноября 2026, Europe/Moscow**. Product decision принят пользователем; interfaces, lifecycle additions и roadmap ниже — **PROPOSED**, implementation отсутствует. Это исправление существующего docs-only PR #3, не новый аудит и не изменение Rust migration M0–M10.

## Обязательное решение

**DECISION: Native ANSA multi-agent orchestration via direct SIWC Responses. No Codex runtime dependency.**

Все роли — Planner, Builder, Reviewer и Researcher — исполняются собственными native ANSA sessions. Каждая использует обычный ANSA agent loop, собственные ToolRegistry/ToolSnapshot и process/file tools, свой session journal и reasoning continuity. Inference проходит только через собственную auth subsystem ANSA и официальный direct SIWC Responses route с ChatGPT plan usage. Coordinator, verification и integration — deterministic code.

**Codex отвергнут как компонент runtime продукта.** Он допустим исключительно как внешний инструмент разработчика для написания исходников ANSA либо необязательный внешний development baseline. Его installation, runtime interfaces, agent state и credentials не требуются ни одному этапу O1–O4. Альтернативный платный inference backend в этот product scope не входит; смена billing route автоматически запрещена.

MVP: один coordinator, один native builder в `basic`, fresh native reviewer в `plan`, read-only planner/researcher и обычный VerificationRunner. Активная role session одна за раз; builders одновременно — **один**. Дочерним sessions не предоставляются orchestration/delegation tools. После eval возможно максимум два builders; рекурсивное создание агентов не входит в MVP.

Default production implementation — отдельная feature после verified M10 GO. Реальные зависимости: Rust protocol/admission, auth, session journal/replay, runtime owner, process/file tools, command/event bridge и их offline/live gates. Внешний coding runtime не является prerequisite. M0–M10 остаются неизменными; narrow non-production composition spike после M6 возможен отдельным scope.

### Статусы и deliverables

| Метка | Значение |
|---|---|
| CONFIRMED | Проверено repository code, GitHub metadata или официальным SIWC contract. |
| TESTED | Проверено запуском; платформа и предел результата указаны. |
| PROPOSED | Новое решение/контракт; не существующее API и не выполненный runtime. |
| UNKNOWN | Требует будущего допуска/измерения; не подменяется предположением. |

Все новые Rust types и CLI shapes ниже — sketches. Здесь нет live model discovery, inference, чтения реальных credentials, native X13s orchestration acceptance или измеренного выигрыша multi-agent. Сохранён исходный repository audit; повторная проверка correction подтвердила прежние main/PR #2 refs.

Пакет: этот отчёт — D1–D5, D7–D8; [четыре prompts](ORCHESTRATION_PROMPTS.md) — D6; [ADR 0002](adr/0002-self-hosted-orchestration.md) — обязательное product decision и предложенные implementation contracts.

## D1. Current State Audit

### Проверенный GitHub snapshot

Репозиторий исследован через GitHub connector. Immutable main: `ca851c99d4887a121300f0bcee8687ed9d055071`. При повторном использовании отчёта сначала обновить snapshot; наличие этого документа не доказывает, что ветки всё ещё стоят на тех же SHA.[A1][A2]

| Объект | CONFIRMED состояние на дату аудита |
|---|---|
| `main` | Merge PR #1; production C++20. Rust workspace в main отсутствует. |
| PR #1 | Merged 12 ноября 2026 в 23:44:05 МСК; архитектура Rust migration, docs-only. |
| `docs/rust-migration-review-20261112` | `91532315e6649eab06baf7628562a78d8e2bac5d`. |
| PR #2 / `migration/rust-runtime` | Open, non-draft, не merged; head `c06b1a839695e0690ead1db88c5e2a2df9a198db`; 3 commits, 22 изменённых файла. |
| M0 | Implementation есть в PR #2; **NO-GO**, native X13s acceptance pending. M1 не начат. |
| GitHub evidence PR #1/#2 | Formal review submissions/comments не найдены. На head PR #2 — ноль check runs и пустые statuses. Это не опровержение локальных тестов, но и не CI/APPROVED evidence. |
| C++ code last known good | `28eb41f0af449e943811cfbf9a4053f2dd7983ee`; более поздний docs merge не является новой runtime acceptance. |

Прочитаны `AGENTS.md`, `PROJECT_STATE.md`, `architecture.md`, `RUST_MIGRATION_PLAN.md`, `TODO.md`, `README.md`, implementation report, contracts/runtime/session/journal/process/provider/auth code и соответствующие tests. `docs/migration/M0.md` и Rust scaffold прочитаны **на PR #2 head**, а не приписаны main.[A3][A4][A5][A6][A7][A8][A9][A10][A11][A12]

Rust scaffold содержит два local packages и ноль external crates. `ansa-rs` намеренно завершается exit 1 с сообщением об отсутствующем runtime; `ansa-linux` — placeholder. Реальных Rust owner, provider, journal и ProcessRunner ещё нет.[A11]

M0 report сообщает x86_64 Debug/Release CTest **86/87**, failure `process-detached`, preflight **10 PASS / 1 UNSUPPORTED**, незакрытый sanitizer/native/performance gate. Исторический C++ отчёт сообщает native **87/87** Debug/Release и **85/85** ASan/UBSan. Это разные checkpoints и среды; старые числа не закрывают M0.[A4][A10]

**TESTED в этом исследовании:** неизменённые пять Python regression tests M0 harness, полученные с PR #2 head, прошли **5/5** на Linux/x86_64, Python 3.12.14; Git blob hashes трёх файлов сверены. Это проверяет anti-false-green assertions harness, не Rust runtime и не native M0. Ни auth, ни inference не запускались; native orchestration этим результатом не проверяется.[A12]

### Что уже можно переиспользовать

| Подсистема ansa | Существующий контракт | Что добавить для Orchestrate |
|---|---|---|
| Runtime owner | Один mutator, owned work identity, bounded inbox/outbox, reserved Cancel, snapshot/resync. | Task/attempt lifecycle и repository ownership; не второй mutator SessionState. |
| Commands/events v1 | Session/Turn/Request/Operation IDs, incarnation, cursor, Submit/Cancel/Resume. | Отдельные orchestration commands/events с feature/version negotiation. |
| Responses admission | Успешный terminal, EOF/transport admission, native replay, whole-batch validation до tools. | Validation `PlanProposal` и `ReviewReport`; они не меняют task state сами. |
| Session journal | Append-only framing/hash chain, flock, fsync barriers, exact epochs, corruption fail-closed. | Отдельный versioned orchestration aggregate с тем же storage machinery. |
| Process supervision | Fresh supervisor/subreaper, consumed marker, durable evidence, stop/reconcile. | Связать owned native child lifecycle с существующим process cleanup/recovery; не создавать внешний coding runtime. |
| Own SIWC | Своя registration, refresh/leases, account binding, официальный model catalog. | Общая auth subsystem, per-session account/epoch binding и доступ к direct SIWC только через provider. |
| basic/plan | Многошаговый loop; plan получает read-only tools без shell. | Builder использует тот же basic loop; planner/reviewer — отдельные plan sessions. |
| M0 harness | Hashes, argv, raw logs, actual test inventory, запрет пустого/skip PASS. | Версионированные acceptance manifests и runner receipts на конкретный SHA. |

Основания: actual contracts и implementation, не предполагаемая функциональность.[A5][A6][A7][A8][A9][A12]

**Настоящая граница:** нынешний `AgentRuntime` обслуживает одну session и один active turn. Он уже решает «model → tools → continuation», но не «goal → task graph → independent acceptance → integration». `Done` — окончание agent turn. Это не принятый milestone.

Особенно важно: C++ `ProcessRunner` может вернуть `Execution::succeeded + Effects::known_result` при nonzero exit, timeout или cancellation, если исход процесса и cleanup известны. Эти признаки находятся внутри output. Код orchestrator не должен превращать такое значение в инженерный success.[A8]

Текущий `run_command` ограничен десятью минутами и 16 KiB сохранённого вывода. Builder использует этот собственный compiled process tool; запуск дочернего AgentRuntime не является shell command. VerificationRunner может использовать те же process primitives с отдельными timeout/output limits и root-owned evidence.[A8]

Отсутствуют TaskId/AttemptId, task dependencies, budgets across sessions, Git worktree ownership, integration queue, independent reviewer gate, acceptance evidence на code SHA, automated selection следующего milestone и общий reconciliation workflow. Текстовое «следующий спринт — orchestrate» в старом PROJECT_STATE/README не отменяет merged migration gates.[A3][A4][A6]

## D2. Integration Decision

### Единственный product execution path

| Слой | Решение |
|---|---|
| Task control | Собственный Rust orchestration owner/reducer, bounded queues и durable task journal. |
| Planner / Researcher | Native `plan` session, read-only ToolSnapshot и отдельный journal. |
| Builder | Native `basic` session с own process/file tools и отдельным worktree. |
| Reviewer | Новая native `plan` session, scoped immutable evidence, без builder conversation. |
| Model inference | Только direct SIWC Responses через собственную auth subsystem ANSA. |
| Verification / integration | Обычные host jobs; LLM не создаёт PASS или merge authority. |
| Storage | Существующие native session journals + отдельный task aggregate + Git/evidence. |

Это композиция существующего engine. Orchestrator не реализует второй model/tool loop и не выбирает между внешними execution backends. Узкая native child factory нужна только для создания/открытия обычного runtime, владения lifecycle и receipt correlation; пустая framework abstraction под отвергнутые backend не нужна.

### SIWC contract и reasoning continuity

**CONFIRMED:** собственный разрешённый OAuth grant допускает ChatGPT plan usage через официальный `https://api.openai.com/v1/responses`. Требуются eligibility, workspace policy и permission `chatgpt.tokens.use.direct`; чужие credential stores не используются.[O1][O4]

Сохраняются `store:false`, `stream:true` и explicit input history. Для HTTP не применяется `previous_response_id`; серверное conversation storage не заменяет local journal. Серверный `multi_agent` не требуется и не поддержан этим SIWC profile. Unsupported поля, включая `background`, `conversation`, `max_output_tokens`, `max_tool_calls`, не добавляются. В MVP модель получает только разрешённые local tools; hosted effectful tools отсутствуют.[O2][O3]

Каждая session сохраняет provider-native items **полностью и в порядке**, encrypted reasoning, call/result IDs, requested/returned model и exact ContextEpoch. Существующий strict Responses admission требует допустимый terminal, корректные EOF/transport и полную batch validation до tools. Незавершённый, отвергнутый или malformed response запускает ноль tools из этого response. Root не синтезирует fake tool outputs для удобного resume и не переносит opaque reasoning между ролями.[A5][A9]

Различие recovery: потерянный direct model response сам по себе не означает неизвестный локальный tool effect — admission мог не разрешить ни одного executor. Повтор generation допустим только из подтверждённого durable state, после завершения прежнего transport, без pending unknown tool intent и в бюджете. Такой retry может повторно расходовать quota; exactly-once inference billing не обещается. Если tool уже был admitted и dispatched, применяется native intent/outcome recovery, а не повтор model call вместо reconciliation.

Structured output — только после route-specific capability validation. Даже schema-valid plan/review остаётся недоверенным proposal. При неподтверждённой feature достаточно bounded JSON/text и строгого local validator; максимум одна corrective generation в общем budget. Второй agent SDK/managed harness не добавляется.[O8]

### Model discovery и расходы

При разрешённом live deployment auth subsystem запрашивает account-specific `GET /v1/models` тем же grant/route: SIWC `models[]`, `slug`, `display_name`, `visibility`. Runtime выбирает capability profile и при разрешённом smoke request требует admitted successful terminal. Catalog/успешный inference подтверждают разные вещи; model/effort не угадываются по статическому списку.[A9][O2]

**UNKNOWN:** actual model entitlement пользователя, supported effort/settings и quota. Реальные credentials здесь не читались. Это O4 deployment gate.

ChatGPT subscription и SIWC plan usage не являются обычным API-key billing. В данной архитектуре API-key fallback отсутствует. При `subscription_sharing_usage_limit_exceeded` новые calls приостанавливаются; error code не определяет reset time и не доказывает исчерпание всей подписки. Transient unavailable допускает bounded backoff; unsupported body, account mismatch и invalid grant требуют остановки/диагностики без циклического login.[O4]

Собственный loop позволяет считать реальные provider dispatches, retries, compaction calls и tool rounds. Текущий `CompletedResponse` не предоставляет полноценную usage accounting structure; optional admitted usage observation — отдельное небольшое расширение telemetry, не выдуманное существующее поле. Отсутствующая token/quota метрика остаётся `unknown`. SIWC расход не пересчитывается по API dollar tariffs.[A9][N3]

## D3. Minimal Target Architecture

### Ownership и reuse одного loop

Один orchestration owner меняет task/attempt/budget/acceptance state. Каждый native AgentRuntime имеет **своего единственного session owner**, который меняет только собственную SessionState. Parent общается commands/events и импортирует durable receipts; он не пишет child journal и не вызывает child reducer напрямую. Auth subsystem общая, mutable conversation state не общий.

В проверенном C++ API класс называется `Runtime`: constructor уже получает `SessionId`, `ContextEpoch`, immutable `ToolSnapshot`, `Generate`, `RuntimeLimits` и `SessionOptions`. Он поддерживает Submit/Cancel/Resume, event stream и shutdown. Термин `AgentRuntime` ниже означает этот engine после Rust parity, а не объявляет новый существующий класс.[N1][N2]

Builder выбирает ordinary `basic` mode; Planner/Researcher/Reviewer — `plan` и read-only tools. Parent не делает самостоятельно generate→prepare tools→continue. Этот код остаётся в native runtime и тестируется теми же basic/plan suites. Новая часть — session preparation, ownership, parent-child correlation, root budgets, handoff и acceptance. Роль не требует нового agent engine или отдельного crate.

### Минимальные typed contracts

Semantic sketches; private constructors для validated/durable values. Native ID semantics и legacy codecs сохраняются.

```rust
struct TaskSpec {
    run: RunId,
    id: TaskId,
    parent: Option<TaskId>,
    depends_on: Vec<TaskId>,        // bounded; cycles rejected
    source: SourceBinding,          // repository + immutable base SHA
    requirements: ArtifactDigest,
    acceptance: ArtifactDigest,
    policy: PolicyRevision,
    budget: BudgetReservation,
}

struct AttemptSpec {
    id: AttemptId,
    task: TaskId,
    owner_epoch: OwnerEpoch,
    expected_task_revision: u64,
    role: NativeRole,
    base: GitOid,
    worktree: OwnedWorktreeId,
    child: ChildSessionBinding,
    handoff: ArtifactDigest,
    tool_snapshot: ToolSnapshotRevision,
    deadline: Deadline,
}

struct ChildSessionBinding {
    session: SessionId,
    store: OwnedSessionStoreId,
    submit: CommandId,
    turn: TurnId,
    request: Option<RequestId>,     // assigned by native runtime at acceptance
    payload: ArtifactDigest,
    configuration: ArtifactDigest,
    initial_context_epoch: ContextEpoch,
    incarnation: Option<RuntimeIncarnation>, // current live handle; changes on reopen
}

enum TaskState {
    Queued, Planning, Ready,
    Executing(AttemptId), Reconciling(AttemptId),
    Verifying(GitOid), Reviewing(GitOid), Integrating(GitOid),
    Publishing(GitOid), Paused(PauseReason), Blocked(BlockReason),
    Accepted(AcceptanceReceipt), Cancelled(CancellationReceipt),
}

enum ChildTerminal { Completed, Cancelled, Paused, Failed, Missing }
enum CleanupState { ConfirmedStopped, Uncertain }

struct ChildReceipt {
    binding: ChildSessionBinding,
    current_context_epoch: ContextEpoch, // reconstructed by native replay/compaction
    terminal: ChildTerminal,
    durable: SessionReceiptRef,    // verified journal receipt, not frontend cursor
    effects: Effects,             // reuse native not_started/known_result/unknown
    pending_operations: Vec<OperationId>,
    cleanup: CleanupState,
    artifacts: Vec<ArtifactDigest>,
}

impl NativeChildSessions {
    fn create_new(&mut self, spec: ValidatedChildSpec,
                  tools: Arc<ToolSnapshot>) -> Result<OwnedChild, ChildError>;
    fn open_existing(&mut self, binding: &ChildSessionBinding,
                     permit: ReconcilePermit) -> Result<OwnedChild, ChildError>;
}

struct VerificationReceipt {
    candidate: GitOid,
    manifest: ArtifactDigest,
    toolchain: ArtifactDigest,
    outcomes: Vec<CommandOutcome>,
    logs: ArtifactDigest,
    source_unchanged: bool,
}

struct ReviewReport {
    base: GitOid,
    candidate: GitOid,
    requirements: ArtifactDigest,
    evidence: Vec<ArtifactDigest>,
    verdict: ReviewVerdict,
    findings: Vec<Finding>,
    unverified: Vec<RequirementId>,
}
```

`OwnedChild` owns обычный runtime handle, bounded event drain, join/result lifecycle и cancellation linkage. Он предоставляет существующие typed command operations, не второй execution engine. Не блокировать orchestration owner на `next_event()` или join: их обслуживает bounded native job/event bridge. Drop не является receipt о quiescence. Размер task graph, children, queues и artifacts ограничен policy.

Initial configuration/epoch в binding неизменны. Текущую epoch после native compaction восстанавливает session reducer; receipt отдельно содержит её и durable checkpoint. Reopen проверяет исходный init и всю допустимую цепочку переходов, а не ошибочно требует равенства текущей compacted epoch исходной. Замороженные instructions/settings/tool revision сохраняются как approved inputs; подмена их свежим содержимым изменённого worktree не является compatible resume.

**Identity precision:** native `RequestId` сейчас принадлежит accepted Submit и может иметь значение `request_1` в разных sessions; это не глобальный ID и не ID каждого HTTP round. Parent key включает SessionId + CommandId + TurnId + RequestId. Provider exchanges учитываются отдельным local ordinal/try + input/epoch digest без переопределения v1 RequestId. Native OperationId и provider call_id сохраняют разные роли.[A9][N1][N2]

### A. Native child lifecycle

1. **Prepare task.** Parent валидирует plan, dependencies, scope, policy и budget. Записывает TaskSpec/AttemptSpec и заранее выделенные SessionId/TurnId/Submit CommandId. Для builder выделяет owned worktree от exact base; для read-only роли — immutable source/evidence view.
2. **Prepare native configuration.** Через те же ANSA settings/instruction builders формирует per-child cwd, role instructions, model/account binding, exact ContextEpoch и ToolSnapshot. Не передаёт модельный TaskSpec напрямую как filesystem path/command. Submit text содержит bounded handoff и artifact references; текущий Submit ограничен 16 KiB.[A9][N4]
3. **CreateNew.** Создаёт own child store вне writable worktree, durable session init/configuration и binding receipt. Только root-owned path, новый ID, один holder session lock. Constructor/creation не получает разрешения самостоятельно отправить goal до parent intent.
4. **Durable submit intent.** После child init parent sync-ит `ChildSubmitIntent` с полным binding и exact payload digest. Затем отправляет обычный RuntimeCommand Submit с текущей child incarnation. `post(...)=accepted` означает только queue admission; оно ещё не доказательство durable acceptance.
5. **Native acceptance.** Child assigns RequestId, sync-ит native `accepted`, затем публикует CommandAccepted. Parent durably импортирует этот receipt. Child может начать loop до root import: root pre-recorded intent уже содержит точную session/command identity, поэтому lost ACK не оставляет неидентифицируемого execution. Продолжение child проверяет выданный revocable dispatch permit и root budget.
6. **Observe and wait.** Event bridge независимо дренирует events; previews служат UI. Done/Paused/RecoveryRequired — observations, которые parent проверяет через durable receipt и tool evidence. `ResyncRequired` вызывает snapshot/receipt reconciliation; frontend sequence не journal offset.
7. **Close and reconcile.** Completed/Cancelled закрывают дальнейший Submit/Resume для attempt; parent ожидает join/cleanup и сохраняет полный inventory. Paused/Missing приостанавливают dispatch и требуют reconciliation: только после gates parent может выдать новый bounded Resume permit той же attempt. Для проверки partial candidate child сначала закрывается и writers останавливаются; после freeze в этот candidate больше не пишут. Только quiescent known state может стать frozen candidate. Worker text «Done» не меняет acceptance.

Предлагаемый durable receipt query использует existing session reducer/receipt data; это небольшой post-migration typed read/export contract, а не парсинг UI/Markdown. Existing CommandReplayed и snapshot помогают correlation, но root должен иметь verified durable identity/configuration/outcomes, включая случаи потерянного event stream.[N1][N2]

### B. Разные контексты, инструменты и общая auth subsystem

У builder и reviewer разные SessionId, journal directory, history, instructions, exact epoch, role ToolSnapshot и owned job. Reviewer никогда не продолжает builder session, не получает её encrypted reasoning и не наследует свободную tool authority. Первый review pass получает requirements/diff/source/evidence без builder summary; summary можно передать вторым материалом.

Per-child ToolRegistry/ToolSnapshot создаётся собственными ANSA factories. Копировать parent closures нельзя: нынешние local tools захватывают settings/cwd, session operations directory, instruction epoch и ProcessRunner. Нужны correct child-specific instances и read-only projection. Все пути задаются явно; изменение process-global cwd ради child запрещено. Exact config включает cwd/instructions/model/tool revision, поэтому изменённый worktree/role не может молча открыть старый context.[N4]

CONFIRMED: текущие tools включают file reads/search и `run_command`; отдельного compiled write-file tool в проверенном baseline нет. Builder уже может менять файлы через собственный process tool обычного basic loop. Дополнительный typed write tool не является условием MVP; если вводится, он обязан иметь native durable intent/recovery contract. Current full-access path resolver допускает absolute paths вне cwd: worktree не становится enforceable write scope автоматически.[N4]

Native reviewer не получает shell/process/write tools. Host экспортирует scoped evidence либо предоставляет узкий `ReadEvidence(ArtifactId)` с host-validated IDs, byte bounds и immutable source snapshot. Read-only flag сам по себе не даёт permission читать protected journals/credentials; этот export позволяет не расширять reviewer до full access.

Auth subsystem разделяет только grant/account lifecycle, catalog и transport access. У каждой session собственный authorization binding; токены не входят в TaskSpec, prompt, journal, ToolSnapshot descriptor или output. Только native provider берёт short-lived access lease. Модель и process/file tools не получают OAuth token API; shell environment фильтруется от secrets.

Текущий auth contract сериализует store/refresh и удерживает leases на время HTTP/tool execution. Это необходимо сохранить при Rust parity. **MVP выполняет roles последовательно**, не обещает concurrent inference с сегодняшней lock discipline. Не брать outer auth lease на всю child session, не ждать root/child permit под auth lock и не запускать дочерний runtime как tool при удерживаемом tool lease. Provider dispatch получает budget permit **до** auth lease; зависимость locks должна быть ацикличной.[N5]

Refresh existing grant остаётся централизованным. Invalid grant/account switch закрывает affected dispatch; login/logout/new grant требует соответствующей policy. Смена account/model/provider/role/epoch не подменяет старую history: explicit new session/handoff либо существующий строго разрешённый native resume. Совпадение auth account не объединяет contexts.

### C. Durable recovery и отсутствие повторных effects

**Два domain authority:** root task journal хранит delegation intent/binding/budget; child session journal хранит accepted Submit, native responses, per-tool intent/outcome, continuation и terminal. Между ними нет притворной atomic transaction. Root импортирует `ChildReceipt`/evidence по exact binding и digest после проверки; повтор import идемпотентен по receipt identity.

Recovery сначала берёт repository/root lock, закрывает new dispatch и проверяет outstanding children. Порядок:

1. Проверить оригинальный child store, session init/configuration и сохранённые Task/Attempt/Command/Turn IDs. Использовать **OpenExisting**, который никогда не создаёт missing/empty store. Existing C++ constructor может создать init для empty journal; такой fallback запрещён recovery wrapper. Отсутствие или конфликт старого store — Blocked, не новая session.
2. Подтвердить, что прежний runtime/локальные writers остановлены; session lock не доказывает остановку всех process descendants. Не полагаться на прежний PID без boot/start identity.
3. Открыть native journal тем же reducer и ToolSnapshot revision. Reopen восстанавливает durable receipts/outcomes и делает unfinished turn paused. Root не посылает Resume при открытии автоматически.
4. Найти accepted Submit по сохранённому CommandId и exact payload/turn/mode. Lost CommandAccepted восстанавливается из того же store. Нельзя выдавать новый CommandId/SessionId как workaround. Существующая dedup exact Submit действует только в этом session store; повторить тот же Submit можно лишь после reconciliation, установив отсутствие acceptance либо получив replay receipt, без конфликтующего active turn.[N1][N2]
5. Native tool recovery проверяет исходный OperationId, invocation/spec hash и evidence. Known outcome сохраняется child runtime до продолжения; root импортирует его отдельно. Полный known result не превращается в новый tool execution. `effect_unknown`, missing/conflicting evidence или неподтверждённый cleanup блокируют Resume и новую write attempt.
6. Если native state paused, effects известны, resources stopped и budgets/policy/config совпадают, parent durably записывает разрешение на **bounded Resume** того же RequestId с новой command ID и текущей incarnation. Completed/Cancelled turn не resubmit-ится как restart. Repair — новый AttemptId и fresh native session только после reconciliation старого.

У native loop уже есть per-tool durable intent→execution→outcome и whole-batch validation. После crash между intent и outcome доказательство ищется через recover; отсутствие доказательства не означает «tool не запускался». Session reducer запрещает continuation при unknown operation. Эти contracts сохраняются, а не дублируются root engine.[A7][A8][N1][N2]

Late known outcome относится к исходным session/attempt/operation и сохраняется, даже если parent cancellation уже принята. Новый owner сверяет старую receipt с **сохранённой epoch исходного AttemptRecord**, затем делает import под своей epoch. Старый callback не может запустить следующее действие; valid outcome не отбрасывается просто из-за смены owner.

Raw HTTP timeout без admitted response допускает ограниченный regeneration только если journal подтверждает отсутствие pending tool effect; уже исполненные tools предыдущих rounds replay-ятся как recorded outputs. Partial SSE previews никогда не записываются как completed response. Corruption/storage failure закрывает новые effects, а не исправляется overwrite. Exactly-once произвольного shell или remote publication не обещается.

### Cancellation, child crash и machine restart

Parent Cancel немедленно закрывает root dispatch latch, затем persist-ит cancellation intent. Связанный child guard прекращает новые generate/tool dispatch, включая окно до durable RequestId. Когда RequestId известен, используется обычный native Cancel с текущими SessionId/TurnId/incarnation. До этого owned-child startup/shutdown path завершает queued submit безопасно и reconciles acceptance; отсутствие RequestId не даёт права оставить child без отмены.

Cancel → stop token → native ProcessRunner cleanup → join → verified receipt. Native Done/Cancelled/frontend snapshot сами по себе не доказывают, что writer quiescent. Известные поздние результаты записываются, но больше не вызывают continuation. При disk failure latch действует немедленно, durable ACK не выдумывается.

Native child живёт как owned runtime job внутри ANSA process, не как внешний agent executable. При parent process crash in-process roles прекращаются; их shell tools обслуживает существующий supervisor/evidence механизм. Current control-pipe EOF baseline — stop-and-reconcile. Supervisor death и detached descendants могут оставить cleanup unknown: тогда worktree/repository write access остаётся quarantined для всех root runs, включая новый RunId.[A8]

После machine reboot используются original journals, durable binding и boot identities. Reopen не равен Resume. Terminal disconnect либо отсоединяет UI от обычного owner service, либо приводит к foreground shutdown с тем же recovery path; непрерывная работа после parent crash не обещается. Дополнительный systemd/cgroup containment возможен после migration, но не является искусственным prerequisite native composition и не добавляется в M6 parity. Его гарантии требуют native tests; cgroup не доказывает rollback remote effects.[L1][L2]

### State transitions, budgets и backpressure

| Переход | Обязательное условие |
|---|---|
| Planning → Ready | Validated plan/dependencies, frozen requirements/manifest, approved scope/budget. |
| Ready → Executing | Durable child init/binding + ChildSubmitIntent + owned native session admission. |
| Executing → Reconciling | Native terminal, cancellation, crash, timeout, transport/storage failure. |
| Reconciling → Verifying | Known/reconciled effects, confirmed local quiescence, full inventory и immutable candidate. |
| Verifying → Reviewing | Required tests реально прошли на exact candidate SHA; no required skip/empty PASS. |
| Reviewing → Integrating | Нет blocking findings/gaps; validator принимает ReviewReport, не строку «approved». |
| Integrating → Publishing | Required tests/review evidence относятся к final integration SHA. |
| Publishing → Accepted | Durable PR/patch receipt и final-SHA acceptance. Accepted не означает main merge. |
| Active → Paused/Blocked | Quota/auth, missing evidence, effect_unknown, cleanup uncertainty, corruption или budget exhaustion. |
| Reconciling → Cancelled | Local writers остановлены; receipt сохраняет known effects и явные external unknowns. |

MVP defaults: initial build + максимум две repairs, максимум одна replan и один заранее разрешённый model escalation, bounded transient retries, один builder. Для повторного failure signature без прогресса — stop. Все Resume, provider calls, compaction и retries учитываются root ledger across sessions/restarts; нативные segment limits сами по себе недостаточны, поскольку Resume может начать новый segment.[N1][N2]

Parent выдаёт child ограниченный execution permit; native dispatch hook проверяет отмену/identity и получает durable provider-call reservation до HTTP. Этот guard оборачивает существующий generate/dispatch boundary, не реализует новый loop. Reserve, потерянный из-за crash до достоверного usage, не возвращается автоматически. Tool intent/outcome authority остаётся child journal; root не создаёт вторую запись истинности каждого tool.

Owner не блокируется на network, auth I/O, journal fsync, filesystem, event drain или join. Bounded jobs/queues сохраняют reserved Cancel. Text previews coalesce-ятся; durable receipts не теряются в UI queue. Event stream resync восстанавливается из native snapshots/receipts, а не повторным execution. Native SSE parser и process stdout/stderr caps сохраняются; отдельные bounded artifacts для verification logs не требуют отключать общий tool cap. Oversized/malformed обязательный evidence — явный failure, не silent truncation с PASS.

### Canonical persistence и migration contract

Предлагается отдельный versioned orchestration journal namespace поверх будущих Rust framing/fsync primitives. Legacy `OMHJ1` session reducer имеет закрытый набор records; task events не вставляются в него под видом совместимого расширения.[A7]

| Данные | Единственный authority |
|---|---|
| Tasks, attempts, dependency graph, policy, budgets, publication/acceptance | Orchestration owner + task journal. |
| Child accepted Submit, native history/reasoning, tool intents/outcomes, session terminal | Native session owner + собственный session journal. |
| Source code | Git objects и проверенный inventory. |
| Mechanical evidence | Trusted runner artifacts; root импортирует immutable receipt. |
| PROJECT_STATE, UI, indexes | Derived handoff/projection, не coordination database. |

Per-repository lock находится выше run directories и относится к Git common-dir. Один root владеет tasks; каждый native journal имеет свой lock. Parent хранит ссылки на child stores вне writable worktree; не объединяет их bytes и не редактирует SessionState. Crash между child append и root import оставляет recoverable orphan receipt; missing artifact при записанной ссылке блокирует acceptance. Frontend cursor не является journal offset/hash.

Новый task namespace появляется отдельной feature; M4 legacy session schema не переписывается и второй canonical session store не появляется. Старый C++ не продолжает новые orchestration tasks. Rollback отключает orchestration dispatch, reconciles children и сохраняет наиболее свежие journals/credentials/evidence; rollback старого journal после effects или rotating credentials запрещён. Migration parity tests сохраняются.[A3]

### Git, ownership и integration

Каждая write-capable попытка получает worktree от immutable base commit. Branch naming, например `ansa/<run-id>/<task-id>/a<attempt>`; name генерирует runtime и проверяет Git ref rules. Integration branch — `ansa/<run-id>/integration`. Default branch не checkout/merge target worker. Обычный Git worktree разделяет часть repository metadata; это удобная изоляция рабочих деревьев, **не sandbox**.[G1]

Правила MVP:

1. Идентифицировать repository/common-dir, default branch и exact base SHA. Нельзя считать текущий dirty user checkout целевой base. Сохранять его изменения; без stash/reset/clean. Для default committed baseline достаточно отдельного worktree. Если задача должна включать WIP пользователя, нужен отдельно разрешённый snapshot; не угадывать.
2. Один write owner на worktree. Новая попытка начинает с явно выбранного reconciled commit; старый worktree сохраняется до retention/cleanup policy. Worker не делает push/merge и не изменяет global Git/security config.
3. После quiescence снять **весь** tracked/untracked inventory, включая deletions/renames. Неожиданные изменения, submodule state и выход за write scope блокируют candidate; их нельзя скрыть, закоммитив только удобные paths. Generated outputs классифицируются заранее; secrets не включаются в artifacts.
4. Host создаёт immutable candidate commit/tree. Host Git commands используют explicit config и controlled hooks/filters; не исполняют неожиданную пользовательскую automations цепочку. Проверяется common-dir/ref identity. Нельзя считать shared worktree locks защитой от hostile same-UID shell.
5. Verification запускается в отдельном чистом worktree exact candidate. Build directories изолированы; shared caches разрешены только по явному безопасному policy. Source inventory до/после tests должен совпасть: тест, изменивший проверяемые sources, не даёт PASS.
6. Integration выполняется последовательно. Для MVP предпочтителен один host-created candidate commit на task и fast-forward/простое cherry-pick в собственную integration branch. Не rebase пользовательских/опубликованных branches и не force-push. Conflict превращается в отдельную bounded integration-repair attempt, с новым base и повторным review/tests.
7. Изменившийся target base делает integration evidence устаревшим. Для нового integration SHA повторяются обязательные gates; старый PASS нельзя переносить на другой SHA. Не гоняться за постоянно движущимся main бесконечно: после bounded rebase/reintegration — StaleBase/Blocked.
8. Публикация касается только разрешённой branch/PR. Merge в main/release — отдельная policy/action. Перед push/create PR journal хранит intent с repo/head/base/run marker. Lost ACK → query exact remote ref и existing PR, проверить SHA, импортировать receipt; не создавать дубликат вслепую.
9. Cleanup удаляет только verified ansa-owned worktree без живых процессов, незасохранённого diff и удерживаемых evidence. Не вызывать `rm -rf` по модельному пути; сохранённые uncertain worktrees не удалять по TTL без разбирательства. Background Git GC не должен удалять нужные candidate objects: удерживать refs/retention pins.

Для двух builders позднее нужны sibling worktrees, declared write scopes и conservative conflict scheduling: lockfiles, workspace manifests, public contracts и cross-cutting files сериализуются. Разные файлы могут конфликтовать семантически — общий contract revision и integration tests всё равно обязательны. Общую архитектурную декомпозицию утверждает owner через валидированный plan.

Non-Git каталог остаётся supported для **basic/plan**, как сегодня. MVP Orchestrate возвращает понятный `GitWorkspaceRequired`; не делает `git init` и не копирует произвольные пользовательские файлы автоматически. Поздняя альтернатива — отдельно согласованный snapshot/patch workflow без обещания PR.

### Независимое review и acceptance

Acceptance требует трёх уровней доказательства:

| Уровень | Кто доказывает | Что сохраняется |
|---|---|---|
| Mechanical | Host VerificationRunner | Actual argv/cwd, toolchain, source SHA, exit/signal/timeout, exact suites/counts/skips и log digests. |
| Behavioral | Requirements assertions + fresh reviewer | Coverage каждого requirement, negative cases, invariants, missing regressions и reproducible findings. |
| Integration | Host integrator + verification/review на итоговом SHA | Точное сочетание base/tasks, resolution diff, fresh receipts и PR head SHA. |

До builder замораживаются original requirements и acceptance manifest: команды, required test IDs/suites, expected environment и scope. Builder может добавлять tests; не может сам отменить required suites, заменить assertions более слабыми, занизить test count или править эталон ради зелёного результата. Исправление ошибочного oracle требует независимого safety argument и отдельного policy decision. M0 harness уже содержит полезные anti-false-green conventions; его `check` не означает весь M0 GO.[A12]

Reviewer получает scoped immutable evidence packet: исходную цель, constraints, base/candidate SHA, полный actual diff, readable source snapshot, acceptance manifest, raw/parsed verification results, предыдущие finding IDs и known limitations. Native plan tools не могут запускать shell и читать protected session roots; owner сам экспортирует разрешённые evidence или даёт узкий `ReadEvidence(ArtifactId)` tool. Не расширять reviewer до full access ради `git diff`.

Первый review pass выполняется без builder self-summary, чтобы уменьшить anchoring; summary можно дать вторым материалом. Reviewer обязан указать file/line или requirement/evidence ID, impact, воспроизведение и необходимую regression. Host проверяет schema/IDs/SHA, но не считает формально валидный JSON доказательством correctness. Missing required evidence = gap, не PASS.

Отдельная сильная модель может уменьшить корреляцию ошибок и полезна для process/replay/auth/storage изменений. Для MVP достаточно отдельного запуска той же сильной модели с чистым контекстом и иным набором инструментов. Это **независимость процесса и evidence**, не статистическая независимость моделей. Она не заменяет native fault tests, а использование другого model name само по себе ничего не доказывает.

Repair loop ограничен: finding → regression/обоснованный fix → новое frozen candidate → проверки → fresh review. После повторяющегося failure без прогресса, истечения quota или максимума attempts — Blocked с artifact links. Нельзя бесконечно менять одновременно код, критерии и тесты, пока они не станут зелёными.

### Memory, handoff и compaction

| Слой | Где хранится / кто обновляет |
|---|---|
| Engineering instructions | Versioned AGENTS/ADR; отдельный reviewed change, модель не расширяет policy. |
| Project state | Git + PROJECT_STATE; integrator после принятого checkpoint, verified/WIP/blocked отдельно. |
| Task state | Только root task journal/reducer. |
| Native agent context | Свой child session journal; exact items/reasoning/epoch не копируются в sibling. |
| Handoff | Immutable goal/TaskSpec/requirements/evidence packet от owner. |
| Tool/process/verification artifacts | Bounded durable evidence от native tool supervisor/runner; импорт root. |
| Historical summaries/indexes | Derived data; не замена исходных receipts. |

Новая orchestration session читает repository snapshot, original goal, plan revision, unresolved findings, budgets, pending approvals и последние receipts. Старый ChatGPT conversation не требуется. Продолжение child возможно только с исходными native binding/history/epoch; fresh session получает explicit handoff с facts и limitations, не чужую chain-of-thought.

Используется обычный native compaction path: сохранение exact epoch и recent tail, durable compact result до следующих calls. Текущий budget измеряется JSON bytes; token-aware optimum не доказан. Pinned requirements/invariants/evidence references сохраняются вне summary. Policy compaction выбирается по input/usage/latency/context errors и regression eval, с reserve на tools/output, без произвольного процента. Compaction calls расходуют root budget; failure не меняет task facts. Серверная compaction feature не становится обязательной зависимостью.[A5]

### Security и approvals

Один approved run profile задаёт пути, process/network permissions, root budgets и publication scope. Обычные чтения, worktree edits и разрешённые tests выполняются автономно. Модель может предложить изменение authority, но не выдать себе grant.

| Действие | Default policy |
|---|---|
| Read-only planning/review/research | Scoped native tools, без shell/credential reads. |
| Builder edits/tests | Native basic tools в owned worktree по approved execution profile. |
| Candidate/integration branch | Autonomous в namespace ANSA, вне main/release. |
| Feature push/draft PR | Только если исходный goal/policy уже разрешает repo/remote/action. |
| Main/release merge, force-push | Auto-merge/force-push выключены MVP; explicit separate policy. |
| Удаление данных | Explicit approval; routine cleanup только своих quiescent сохранённых artifacts по retention policy. |
| Login/logout, новые grants/account switch | Explicit approval; normal refresh existing own grant — runtime lifecycle. |
| Security/credential configuration | Отдельное reviewed change; AGENTS не снимает ограничения пользователя. |
| Paid route/services | Вне product scope; quota failure не разрешает новый billing path. |
| Несовместимая storage migration/deploy/irreversible effects | Отдельная operation policy и recovery/approval. |

Full access ANSA остаётся сознательной политикой владельца. Native ToolSnapshot отделяет advertised tools и loop authority, но **не sandbox для произвольного shell**. Собственные process/file tools должны проверять scope там, где это технически возможно; полный inventory обнаруживает нарушения, но не отменяет их. Current path resolver не обеспечивает ограничение одним cwd. Same-UID full-access process может обратиться к другим worktrees, journals, сети/credentials и запустить ещё ANSA process. Нельзя обещать запрет только настройками prompts/registry.[A8][N4]

Строгие guarantees требуют реально enforced OS/filesystem/network/credential boundary. Без неё модель остаётся доверенным full-access coding actor; mechanical evidence защищает от self-reporting, но не от hostile same-UID tampering. Worktree/hash/no_new_privs/cgroup не дают всех этих гарантий. Каждый тест — тоже executable code под утверждённой policy.

Только центральная auth subsystem читает собственные OAuth credentials. Никаких token values в journals/logs/prompts, child specs или tool process environment; credentials других приложений не используются. Если выбранный profile не обеспечивает требуемую boundary, live unattended gate остаётся закрытым. Synthetic sentinel leakage tests возможны без реальных credentials.[N5][O17]

## D4. End-to-End Workflow

### Один пользовательский запрос

Предлагаемый будущий UX: `ansa orchestrate 'Реализуй следующий milestone согласно PROJECT_STATE'`. CLI syntax требует отдельной реализации; сегодня такой команды нет.

1. Owner получает goal и выбранный policy profile. Читает фактические Git refs/dirty state, AGENTS, PROJECT_STATE, roadmap и evidence gates. Создаёт durable RunId. Нельзя считать milestone ready по одной свободной фразе Markdown.
2. Planner в read-only session выдаёт bounded plan, requirements mapping, scope и предложения tests. Deterministic code проверяет зависимости/лимиты/permissions. Неподтверждённая native gate превращается в blocker.
3. Owner фиксирует base SHA, requirements и test manifest, выделяет worktree и budget. Один native builder получает durable child Submit через обычный ANSA command bridge; ручного переноса сообщений нет.
4. Builder редактирует код и может запускать разрешённые проверки для собственной обратной связи. Его сообщение «готово» только завершает attempt. Root выполняет cleanup/reconcile и снимает actual candidate.
5. VerificationRunner выполняет обязательные команды на exact candidate; source mutation, отсутствующий test suite или timeout дают failure. Результаты пишет runner.
6. Fresh reviewer читает actual diff/source/requirements/evidence. Blocking finding становится новым bounded attempt; acceptance criteria не ослабляются.
7. Host интегрирует approved candidate в свою branch, создаёт окончательные docs/handoff и повторяет gates на final SHA. Если после проверки изменились документы/код, появляется новый SHA и evidence пересматривается; PROJECT_STATE не должен содержать выдуманную ссылку на собственный будущий commit.
8. При разрешённой публикации host делает обычный push feature branch и создаёт draft PR. Durable receipt хранит URL/number/head SHA, тесты, findings, usage и limitations. `Accepted` означает готовый проверенный PR/patch; merge main ещё не выполнен.
9. Owner либо завершает согласованный milestone, либо выбирает следующий **только в границах заранее разрешённого run scope, dependencies и оставшегося бюджета**. «Продолжай проект» не даёт бесконечного право на все будущие milestones.

### Что правильный orchestrator сделал бы с ansa сегодня

На зафиксированном snapshot он обнаружил бы открытый PR #2 и незакрытый native M0 gate. Без доступного разрешённого X13s runner он должен вернуть **Blocked: native M0 acceptance pending**, exact PR/head и список недостающих native tests. Не начинать M1 и не объявлять M0 закрытым по x86_64 scaffold results. Такой отказ — корректная автономная работа, а не сбой usefulness.[A10]

### Первая будущая полезная coding-задача

В O4 после offline готовности Orchestrate выбрать маленький реальный кусок уже обозначенной границы unknown-effect reconciliation: **read-only report незавершённых process intents**, без разрешения повторять их. Предлагаемый UX — `ansa session inspect --unresolved --json`; имя команды ещё не принято.

Acceptance: отчёт показывает session/turn/operation identity, известный dispatch/cleanup/evidence status, причину блокировки и необходимые действия; не читает secrets; не изменяет journal; не запускает process. Cases: missing evidence, conflicting spec hash, known late outcome, corrupt/torn evidence, cancelled-before-dispatch и повторный inspect. Добавить integration regression, что получение отчёта не увеличивает счётчик исполненных synthetic effects. Это реальная работа с recovery contracts и отрицательными сценариями, а не косметический README benchmark.[A5][A8]

Пример symbolic trace: `base B → candidate C1 → verification V1 → reviewer F1(missing conflicting-hash case) → repair C2 → V2 PASS → review R2 → integration I → final V3/R3 → PR(P, head=I)`. В живом trace B/C/I заменяются actual Git OIDs; каждый receipt содержит digest. Если к моменту O4 функция уже реализована, выбрать сопоставимую незакрытую задачу из actual TODO, сохранив критерии эксперимента.

### Обязательные failure scenarios

| Событие | Немедленная реакция | Разрешённое продолжение |
|---|---|---|
| Worker умер после записи файлов | Freeze task, stop/reap остатки, inventory/diff, сохранить evidence. | Known local state можно превратить в checkpoint и новый attempt; uncertain remote/cleanup — Blocked. |
| Reviewer отклонил | Записать finding IDs и requirement gaps. | Максимум оставшихся repairs; новый SHA и повтор gates. |
| Tests повторно fail | Сверить signature и прогресс; не менять expected PASS. | Один bounded diagnosis/escalation, затем Blocked; счётчики не обнуляются. |
| Integration conflict | Оставить working branches, запретить publish. | Отдельная repair attempt на actual integration base; новый review/tests. |
| Provider недоступен | Приостановить new dispatch, сохранить состояние. | Bounded direct generation retry только из known durable native state; unknown tool intent не replay-ится. |
| Parent перезапустился | Repository lock/new epoch, dispatch закрыт, old-attempt reconciliation. | Reopen original child store; known native outcomes импортируются; bounded Resume только после quiescence и проверки effects/config/budget. |
| Budget exhausted | Stop new model/worker dispatch; orderly cancellation активного по policy. | Сохранить partial patch/evidence; explicit budget extension, не скрытая смена model/account/API. |
| Cancel после external side effect | Записать уже совершённое действие и unknown часть. | Отдельное reconciliation/compensating action при разрешении; cancellation не rollback. |
| Terminal completed получен, journal append упал | Сохранить volatile result, прекратить effects. | Сопоставить child journal/tool evidence/Git после восстановления; не создавать новую child session автоматически. |
| Lost push/PR response | Publication остаётся pending/uncertain. | Query exact remote head/PR marker; импортировать существующий результат либо Blocked при неоднозначности. |
| Machine reboot | Не полагаться на прежние PID; boot ID изменился. | Reopen newest journal/evidence, проверить файловую целостность; power-loss assumptions не заменять обычным SIGKILL test. |

## Model routing и экономическая модель

**PROPOSED:** native role profile выбирает доступную SIWC capability, а не hardcoded model name. Account/model/effort/config revision входят в child binding; смена не происходит незаметно в старой exact epoch.

| Profile | Критерий выбора |
|---|---|
| High-reasoning planning/research | Зависимости, ownership, архитектура, разделение evidence и uncertainty. |
| Everyday native coding | Accepted-task качество на собственных process/file tools и небольших diffs. |
| Cheap bounded | Проверяемая extraction/classification, без security/acceptance authority. |
| Independent native review | Контрпримеры, actual source/diff, invariants; fresh session обязательна. |
| Escalation | Один разрешённый переход к доступной модели с измеренным преимуществом на данном failure family. |

Разные модели не обязательны: стартовый eval использует одну сильную доступную модель в отдельных contexts. Same-model review сохраняет correlated blind spots; другой model profile полезен по paired eval, не по названию роли. Researcher/recovery analyst — тот же planner в новом bounded task, только после deterministic recovery facts.

| Native конфигурация | Когда полезна | Когда overhead лишний |
|---|---|---|
| Один native basic agent | Ясная локальная задача и готовые acceptance criteria. | Не выполняет весь project-level integration/handoff сам по себе. |
| Planner + один native builder | Необходимо выделить bounded scope и зависимости milestone. | План уже полностью задан. |
| Coordinator + builder + fresh reviewer | Важны negative cases, independent acceptance и recovery. | Тривиальная правка с исчерпывающим mechanical oracle. |
| Planner + два native builders | Реально независимые scope и достаточные CPU/RAM/quota. | Shared contracts/lockfiles, auth serialization, X13s contention. |

Главная метрика — все planning/build/review/repair/verification расходы на одну принятую задачу. Непринятые попытки не исключаются из суммарного расхода. Для SIWC сохраняются actual dispatched calls, provider retries, native tool rounds, resumes, compaction calls, wall time, throttling/errors и optional admitted usage. Usage отсутствует → unknown, не zero. Shared usage других приложений отмечается как confounder; API tariffs не применяются.[O4][O17]

Parent ledger ограничивает attempts, requests/resumes, deadlines, queue/resources и общий reserve. Retry или новая session не обнуляет counters. SIWC не предоставляет нужного per-request output cap; hard in-flight token/dollar ceiling не обещается. Runtime прекращает новые calls при исчерпании reserve, отменяет текущую работу по policy, сохраняет overrun/unknown charge. Возможности собственного loop дают лучший dispatch accounting, но не позволяют выдумывать quota reset или фактическую стоимость.

## D5. Executable Roadmap

### Реальные зависимости от Rust migration

| Module/gate migration | Что переиспользуется native orchestration |
|---|---|
| M2 | Responses codec/admission, provider-native reasoning/items, exact replay. |
| M3 | Собственный SIWC auth, shared lifecycle/leases, account/model binding. |
| M4 | Canonical native session journal, init/accepted/tool intent/outcome/reopen. |
| M5 | Runtime owner, commands/events, bounded jobs и targeted Cancel. |
| M6 | Own process/file tools, ToolRegistry/ToolSnapshot, supervision/recovery. |
| M7 | Headless command/frontend boundary; Orchestrate не требует нового TUI. |
| M8 | Полный offline E2E parity и отрицательные сценарии. |
| M9 | Отдельно разрешённый live/cutover gate. |
| M10 | Rust production checkpoint и завершённый cutover, после которого начинается отдельная feature. |

Эти modules — технический фундамент. Удаление C++ не нужно для создания child session само по себе; **default implementation после verified M10 GO** — sequencing, сохраняющий принятый migration scope. M5 исключает Orchestrate; M6 parity не расширяется cgroups; M10 не совмещает cutover с новой продуктовой функцией.[A3]

| Вариант | Решение |
|---|---|
| A — отдельная feature после M10 | Default: не переносим orchestration дважды и не размываем parity. |
| B — после M8/M9, C++ ещё сохранён | Технически возможен отдельным product decision, если M10 задержан организационно; не подразумевается текущим ADR. |
| C — narrow non-production spike | После M6 можно проверить native composition/reopen на synthetic transport, вне production/defaults/migration PR. Сейчас — только docs/contracts. |

Ни один этап не зависит от установки внешнего coding runtime. O1 стартует от подтверждённого code/evidence checkpoint `migration/m10-rust-production` или его явно принятого преемника, а не от наличия одного tag. M0–M10 и PR #2 не изменяются.

### O1–O4: только Rust ANSA, native sessions и direct SIWC

| Этап | Scope | Acceptance / meaningful tests | Stop / rollback |
|---|---|---|---|
| O1 — durable task control | После M10: root reducer/journal, repository lock, Task/Attempt/child identity binding, budgets; deterministic fake native-child port и scripted verification/review receipts. | Goal→fake child→scripted gates→accepted/blocked; queue acceptance не equal durable acceptance; crash на root boundaries, duplicate/stale receipts, filled queues, cancel latch, restart сохраняет exhausted budget; child completion не даёт Accepted. | Double dispatch или false acceptance — NO-GO. Отключить feature, сохранить state; basic/plan unchanged suites. |
| O2 — native child composition | Reuse Rust AgentRuntime basic/plan, own per-child ToolSnapshot/settings/worktree/journal, shared auth subsystem, native receipt bridge, CreateNew/OpenExisting, cancellation/recovery. Synthetic direct Responses transport; реальные native process/file fixtures без live credentials. | Exact child ID binding; lost CommandAccepted; original missing store blocks; no cross-context reasoning; malformed/incomplete SSE запускает ноль tools; known outcomes replay without execution; effect_unknown blocks Resume/new attempt; parent/child/supervisor death; no auth lease cycle; synthetic secret redaction. | Missing store/identity mismatch, duplicate effects, unconfirmed cleanup или credential leak — NO-GO. Close permits/reconcile, retain child journals/worktrees. |
| O3 — verified native workflow | Native planner/builder/fresh reviewer, host VerificationRunner, frozen manifest, scoped evidence export, serial integration, recoverable publication. Tests используют тот же runtime и deterministic model fixtures/local Git remote. | Required suite zero/skip не PASS; wrong-SHA/modified-source receipt rejected; fresh reviewer has no shell/builder history; bounded repair/Resume and provider calls; conflict/stale-base tests; crash after push/PR reconciles single publication. | False PASS/publish replay — NO-GO. Disable publication, retain patch/evidence, basic/plan fallback. |
| O4 — ограниченный native live pilot | После O3: отдельно разрешённый собственный SIWC profile/model/quota и X13s run/publication policy. Actual route discovery, один useful end-to-end task, native eval и target recovery. | Ни одного ручного message handoff; exact-SHA mechanical/behavioral/integration receipts; native cancellation/restart; measured calls/time/RSS/quota observations; все роли работают без внешнего coding executable. | Unsupported SIWC/native cleanup/security capability или quota exhaustion → Blocked; no route/credential bypass. Retain artifacts, обычный native basic/plan остаётся доступен. |

O1 fake port не превращается в второй engine; O2 подключает существующий runtime через owned composition. O1–O3 полностью offline: fixtures эмулируют тот же direct Responses contract. O4 впервые допускает real SIWC calls по отдельному разрешению. Во всех этапах recovery tests — обязательные gates, не отложенное hardening.

### После MVP

O5 — максимум два native builders после eval и проверки auth contention, independent ToolSnapshots/worktrees, root-wide request/resource caps и serial integration. Если throughput упирается в текущую auth serialization, сначала измерить её; не ослаблять grant/refresh guarantees ради параллельности. Отдельные OS child processes или более сильный sandbox — самостоятельное решение по измеренной необходимости, не второй agent loop.

Если orchestration не улучшает accepted rate/human interventions/regressions относительно одного native agent, оставить простой native path. Нет отложенной «замены основного executor»: native builder обязателен с первого работающего MVP.

## Evals: воспроизводимо проверить пользу

**PROPOSED; live runs здесь не проводились.** Минимальный pilot: четыре реальные задачи × три native serial configurations = **12 runs**. При полезном сигнале восемь задач × три конфигурации × два повторения = 48 runs; parallel subset отдельно. Общий quota/time cap выбирается до начала.

| ID | Product-native конфигурация |
|---|---|
| N0 | Один ordinary native ANSA basic agent; bounded self-checks/repairs. |
| N1 | Native planner + один native builder + host verification. |
| N2 | Native coordinator + builder + fresh native reviewer + host verification. |
| P2 | Только для подходящих tasks: два native builders с теми же N2 gates. |

Codex может участвовать только как **необязательный внешний development baseline**, запускаемый разработчиком вне ANSA orchestration. Он не является backend, prerequisite, dependency или обязательным arm acceptance этого продукта. Product eval полностью исполним без его установки.

Одинаковый внешний final harness/adjudicator оценивает все arms: base SHA, acceptance criteria, tool permissions, model/effort и общий budget совпадают; baseline может делать собственные bounded repairs. Fresh sessions/worktrees, pinned toolchain/resources/cache policy, randomized order, решения между arms не передаются. Benefit planning — N1−N0, review — N2−N1, parallelism — P2−N2.

### Реальные задачи ANSA

| Case | Область и проверка |
|---|---|
| E1 | Unresolved process intents diagnostic: missing/conflicting evidence, zero effects; первый useful O4 candidate. |
| E2 | Long-session compaction corpus: сохранение constraints, exact reasoning/tail/epoch и restart. |
| E3 | Journal/replay negative case: controlled fault или historical fix с явно указанным статусом. |
| E4 | Targeted Cancel при blocked output/storage: late known result, bounded responsiveness. |
| E5 | Process identity/consumed marker: отсутствие duplicate effect после reopen. |
| E6 | M0 harness anti-false-green extension: missing/duplicate/skipped inventory; не подмена M0 GO. |
| E7 | Native transcript pagination/restart boundary. |
| E8 | Независимые fixture/evidence additions: parallel subset только при непересекающихся scope. |

Corpus привязан к actual source/TODO; post-migration paths и exact SHAs фиксируются до runs. Если feature уже реализована, её заменяют до рандомизации. Seeded faults не объявляются новыми production bugs; весь корпус не должен состоять из trivial mutations.[A4][A5][A8][A12]

Метрики: accepted task rate; все calls/observable quota на accepted task; wall-clock; provider retries/repairs/resumes/replans; confirmed findings/false positives/escaped defects; integration conflicts; human interventions; regression rate; recovery success и correct blocked outcomes отдельно; startup/RSS/CPU/FDs/disk/cleanup latency. При zero accepted cost per accepted не равен нулю. UI disconnect и parent restart считаются разными faults.

Offline fault suite: kill на submit acceptance/tool intent/outcome/root import/publication boundaries, full queues, corrupt/missing child store, mismatched scope/epoch, cancelled child, exhausted budgets. Поймать unknown и остановиться — safety success, но не automatic continuation success. При малом n результаты screening, не statistical proof. Если N2 не даёт пользы, не включать его повсеместно; если P2 ухудшает X13s latency, concurrency остаётся 1.

## D6. Prompts

[ORCHESTRATION_PROMPTS.md](ORCHESTRATION_PROMPTS.md) сохраняет имя файла для совместимости ссылок. В нём четыре полных текста: первый O1 implementation для внешнего разработчика; fresh native review; continuation из native state; автоматически составляемый native builder task. Имя файла не означает runtime dependency. Prompts не заменяют typed state/policy и не запускались.

## D7. Project Documentation

В существующем docs-only PR #3 исправлены research, ADR, prompts, AGENTS и связанные README/TODO/PROJECT_STATE. Активное product decision везде native-only direct SIWC; historical development notes не используются как product requirements. Обязательный ADR literal приведён в начале отчёта и ADR. Production source, main, PR #2 и RUST_MIGRATION_PLAN не меняются.

Consistency acceptance: прочитать все семь файлов и полный diff; проверить отсутствие отвергнутых runtime dependencies в active recommendations, одинаковый O1–O4, native identity semantics, CreateNew/OpenExisting distinction, effect_unknown gates, fresh reviewer, direct SIWC и unchanged migration scope. Независимый reviewer проверяет фактический обновлённый diff. Docs checks не являются доказательством реализованного runtime или native M0 GO.

## D8. Risks, Non-Goals и открытые вопросы

| Риск / вопрос | Ограничение и будущая проверка |
|---|---|
| Child receipt bridge отсутствует как orchestration API | O2 typed durable read/export поверх existing reducer; очередь/preview cursor не evidence. |
| Потеря original child store | OpenExisting fail-closed; не воссоздавать journal и не повторять Submit. |
| Cross-context/config leakage | Per-session journal/instructions/epoch/ToolSnapshot; shared auth не shared history. |
| Auth contention / lock cycles | Последовательный MVP, bounded leases; O2 synthetic cancel/refresh tests, O5 measurements. |
| Unknown tool/process effects | Native recovery и durable quarantine, без обхода через новую attempt. |
| Full-access shell | Логическая scope policy не sandbox; строгий enforcement требует отдельной проверенной OS boundary. |
| Usage gaps | Реальные dispatch counters; usage optional/unknown; in-flight hard token cap не обещается. |
| Same-model review | Fresh context + independent negative tests; model diversity по eval. |
| Scope creep migration | Post-M10 default; реальные modules перечислены; M0–M10 unchanged. |
| Disk power loss / supervisor death | Native fault acceptance отдельно; SIGKILL не доказывает power-loss durability. |

UNKNOWN до implementation/live gates: точная Rust child factory/receipt API форма; original-store integrity/retention scheme; behavior всех tools на restart и native X13s cleanup; permit/budget durability overhead; shared auth fairness при двух builders; account model/effort eligibility; safe process environment; token/usage telemetry; long-context quality; measured N2 benefit. Product choice native-only не является открытым вопросом.

**Explicit non-goals:** внешние coding runtimes; большие swarms; recursive unbounded delegation; self-modifying skills; второй agent engine/framework; microservices/Redis/PostgreSQL; неофициальные endpoints/browser automation; зависимость от Pro/старого чата; automatic policy expansion; платный fallback; automatic main merge/force-push/destructive cleanup; exactly-once произвольного shell; неограниченный native Resume; non-Git write orchestration в MVP.

### Однозначные ответы

| Вопрос | Обязательный ответ |
|---|---|
| Product execution | Только native ANSA sessions/loop/tools. Внешний coding tool допустим лишь для разработки ANSA. |
| Inference | Только собственный direct SIWC Responses; local child sessions, без серверного multi_agent. |
| Paid API | Не входит в принятое решение и не является fallback. |
| Разные модели | Не обязательны; доступные SIWC profiles и fresh contexts. |
| Review без Pro | Fresh native reviewer на actual diff/source/runner evidence. |
| Durable child | Original native session, exact binding, known outcome recovery, no replay effect_unknown. |
| Parallel workers | Сначала 1, затем максимум 2 после eval. |
| Continuation | Task journal + original child journals + Git/evidence/budgets/handoff. |
| Migration | Собственные Rust modules M2–M7, gates M8/M9; default feature после M10. |
| Первый E2E | Сейчас корректный M0 blocker; в O4 useful native diagnostic → verified PR. |

## Источники

Repository evidence сохранено из исходного исследования; correction использует те же immutable SHAs. Нового полного аудита не выполнялось.

### ANSA

[A1]: https://github.com/junknewera/ansa/commit/ca851c99d4887a121300f0bcee8687ed9d055071
[A2]: https://github.com/junknewera/ansa/pull/1
[A3]: https://github.com/junknewera/ansa/blob/ca851c99d4887a121300f0bcee8687ed9d055071/docs/RUST_MIGRATION_PLAN.md
[A4]: https://github.com/junknewera/ansa/blob/ca851c99d4887a121300f0bcee8687ed9d055071/docs/PROJECT_STATE.md
[A5]: https://github.com/junknewera/ansa/blob/ca851c99d4887a121300f0bcee8687ed9d055071/docs/architecture.md
[A6]: https://github.com/junknewera/ansa/blob/ca851c99d4887a121300f0bcee8687ed9d055071/docs/TODO.md
[A7]: https://github.com/junknewera/ansa/blob/ca851c99d4887a121300f0bcee8687ed9d055071/src/session.cpp
[A8]: https://github.com/junknewera/ansa/blob/ca851c99d4887a121300f0bcee8687ed9d055071/src/process.cpp
[A9]: https://github.com/junknewera/ansa/blob/ca851c99d4887a121300f0bcee8687ed9d055071/include/omh/contracts.hpp
[A10]: https://github.com/junknewera/ansa/blob/c06b1a839695e0690ead1db88c5e2a2df9a198db/docs/migration/M0.md
[A11]: https://github.com/junknewera/ansa/tree/c06b1a839695e0690ead1db88c5e2a2df9a198db/rust
[A12]: https://github.com/junknewera/ansa/blob/c06b1a839695e0690ead1db88c5e2a2df9a198db/tests/parity/run.py
[N1]: https://github.com/junknewera/ansa/blob/ca851c99d4887a121300f0bcee8687ed9d055071/src/runtime.cpp
[N2]: https://github.com/junknewera/ansa/blob/ca851c99d4887a121300f0bcee8687ed9d055071/include/omh/session.hpp
[N3]: https://github.com/junknewera/ansa/blob/ca851c99d4887a121300f0bcee8687ed9d055071/include/omh/responses.hpp
[N4]: https://github.com/junknewera/ansa/blob/ca851c99d4887a121300f0bcee8687ed9d055071/src/local_tools.cpp
[N5]: https://github.com/junknewera/ansa/blob/ca851c99d4887a121300f0bcee8687ed9d055071/src/auth.cpp

- [Main commit][A1], [PR #1][A2], [PR #2](https://github.com/junknewera/ansa/pull/2), [AGENTS](https://github.com/junknewera/ansa/blob/ca851c99d4887a121300f0bcee8687ed9d055071/AGENTS.md), [migration plan][A3], [PROJECT_STATE][A4], [architecture][A5], [TODO][A6].
- [Session reducer][A7], [journal](https://github.com/junknewera/ansa/blob/ca851c99d4887a121300f0bcee8687ed9d055071/src/journal.cpp), [ProcessRunner][A8], [contracts][A9], [Responses](https://github.com/junknewera/ansa/blob/ca851c99d4887a121300f0bcee8687ed9d055071/src/responses.cpp).
- [Runtime][N1], [native runtime interface](https://github.com/junknewera/ansa/blob/ca851c99d4887a121300f0bcee8687ed9d055071/include/omh/runtime.hpp), [SessionState/SubmitReceipt][N2], [CompletedResponse][N3], [ToolRegistry/ToolSnapshot](https://github.com/junknewera/ansa/blob/ca851c99d4887a121300f0bcee8687ed9d055071/include/omh/tools.hpp), [local tools][N4], [settings/epoch](https://github.com/junknewera/ansa/blob/ca851c99d4887a121300f0bcee8687ed9d055071/src/agent_config.cpp), [auth][N5], [authorization](https://github.com/junknewera/ansa/blob/ca851c99d4887a121300f0bcee8687ed9d055071/src/agent_authorization.hpp).
- [M0][A10], [Rust scaffold][A11], [harness][A12], [harness tests](https://github.com/junknewera/ansa/blob/c06b1a839695e0690ead1db88c5e2a2df9a198db/tests/parity/test_run.py), [implementation report](https://github.com/junknewera/ansa/blob/ca851c99d4887a121300f0bcee8687ed9d055071/docs/implementation-report.md).

### Официальный direct SIWC contract

[O1]: https://developers.openai.com/siwc/token-sharing-open-source
[O2]: https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference
[O3]: https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations
[O4]: https://developers.openai.com/siwc/token-sharing-open-source/errors-and-recovery
[O8]: https://developers.openai.com/api/docs/guides/structured-outputs
[O17]: https://developers.openai.com/siwc/token-sharing-open-source/profiles-and-sessions

- [Own registration/plan usage][O1], [models and inference][O2], [SIWC limitations][O3], [errors/recovery][O4], [accounts/refresh][O17]. Общий [structured outputs contract][O8] не доказывает route-specific entitlement.

### Git и Linux

[G1]: https://git-scm.com/docs/git-worktree
[L1]: https://cdn.kernel.org/doc/html/latest/admin-guide/cgroup-v2.html
[L2]: https://github.com/systemd/systemd/blob/main/man/systemd.kill.xml

- [Git worktree][G1], [cgroup v2][L1], [systemd kill policy][L2]. Дополнительный containment profile — optional post-migration capability с собственными tests.
