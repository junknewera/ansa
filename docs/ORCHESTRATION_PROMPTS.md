# D6 — Prompts для Codex и будущего ANSA executor

**Статус: PROPOSED.** Это текстовые контракты будущих запусков, а не реализация. Названия `CodingExecutor`, `TaskSpec`, `OrchestrationJournal` и полей ниже — предлагаемые interfaces; в проверенном репозитории они не существуют. Все `{{...}}` — обязательные значения, которые deterministic coordinator подставляет из принятого плана и canonical state. Исполнитель не должен угадывать пустые SHA, permissions или budgets.

## 1. Первый implementation prompt — O1, строго после M10 GO

Ты работаешь в репозитории ansa над **первым узким offline milestone Orchestrate O1**. Цель — доказать durable lifecycle одной задачи на fake executor; реального coding agent, UI и сетевых вызовов в этом milestone нет.

Сначала прочитай корневой AGENTS.md, docs/PROJECT_STATE.md, docs/RUST_MIGRATION_PLAN.md, docs/adr/0002-self-hosted-orchestration.md и docs/ORCHESTRATION_RESEARCH.md и scoped AGENTS для выбранного Rust crate. Выполни read-only inventory: актуальный remote/main SHA, текущая ветка/HEAD, git status, существующие migration tags и evidence. Не полагайся на старый чат или утверждение в этом prompt о текущей готовности.

**Обязательный prerequisite: подтверждённый M10 GO.** Нужны проверенный checkpoint `migration/m10-rust-production` или его явно документированный преемник, соответствующий code tree и evidence, а также отсутствие незакрытых migration blockers. Одного tag или scaffold недостаточно. Если фактическое состояние по-прежнему M0 pending native X13s, результат этой задачи — **BLOCKED** с actual main/PR head SHA, ссылкой на незакрытый gate и точным требуемым действием. Не начинай M1, не выполняй M0 вместо этой задачи и не обходи prerequisite переносом Orchestrate в C++.

После проверки prerequisite создай отдельную implementation branch/worktree от согласованного immutable base `{{BASE_SHA}}`. Сохрани все пользовательские изменения; не выполняй stash/reset/force-checkout. Путь Rust crate определи по реальному post-M10 workspace: `rust/` мог быть перемещён отдельным mechanical commit.

Реализуй только минимальный scope O1 по ADR:

- Typed task/attempt/parent IDs, immutable task specification и проверяемые transitions одной задачи.
- Один serialized owner изменяет task state; worker возвращает observations. Repository-level lock исключает второй root run; budgets/repair counts сохраняются через restart. Bounded queues, отдельная адресная отмена, один execution slot.
- Один canonical orchestration journal и reducer для task lifecycle. Используй решение ADR о namespace/version/compatibility; legacy session journal не переписывай. Snapshot/index/Markdown — производные представления, не второй authority.
- Durable execution intent до fake dispatch; consume-once receipt/attempt identity; outcome admission и durable acceptance. Одно событие completion не означает принятую engineering task.
- `CodingExecutor` только с deterministic fake backend. Fake исполняет заранее заданный script outcomes и barriers; verification/review gates получают scripted fake VerificationReceipt/ReviewReport, без реальных runners/LLM. Нельзя переходить в Accepted непосредственно из executor completion; он не запускает Codex, shell writers, native agents или модели.
- Reopen/replay восстанавливает последний durable state. Outstanding execution без достоверного outcome становится `ReconciliationRequired`/`Unknown` согласно ADR и никогда не повторяется автоматически. Cancel блокирует новые dispatch, но сохраняет поздний известный outcome своего attempt.

Не добавляй Git integration, real process supervisor, provider routing, live auth, reviewer LLM, database server, новые production services или плагины. Не меняй M0–M10 и существующие acceptance criteria. Новые crates допускаются только если уже обоснованы ADR; не создавай framework или заранее пустые modules.

Acceptance требует содержательных deterministic tests: duplicate submit, повтор consume receipt, stale attempt/incarnation completion, Cancel до и после durable intent, late known outcome после Cancel, crash/reopen на каждой новой durable boundary, torn tail, append/fsync failure, saturated ordinary queue при доступном Cancel, unknown без второго dispatch. Счётчик fake effects — independent oracle; тест не должен лишь повторять implementation branch logic. Явно отдели исполнимые tests от planned fault cases.

Выполни актуальные locked build/test/fmt/clippy и нужные compatibility gates из AGENTS на exact candidate tree. Зафиксируй команды, toolchain/platform, test IDs/results, failures/skips, source/candidate SHA. Если native gate недоступен, оставь его pending; не выдавай host run за X13s.

Перед завершением передай настоящий diff, original acceptance и raw evidence отдельному reviewer run с чистым контекстом. Исправь максимум два согласованных repair attempts; отсутствие прогресса или новый архитектурный blocker останавливает работу. Итог: reviewable implementation commit, evidence/handoff и обновлённый PROJECT_STATE на final проверенном SHA и список pending gates. Никаких push/main/release mutations без заранее заданной публикационной policy. Возвращай implementation PR только когда эта policy разрешает его создание; иначе branch/patch и точный blocker.

## 2. Prompt независимого review

Ты — independent reviewer результата ansa. Это отдельный запуск с чистым контекстом; summary исполнителя не является доказательством. Твоя задача — найти подтверждаемые нарушения original requirements, архитектуры и acceptance, а не продолжать реализацию.

Входной bundle: original goal и принятой TaskSpec, authoritative AGENTS/ADR, `base_sha={{BASE_SHA}}`, `candidate_sha={{CANDIDATE_SHA}}`, integration candidate при наличии, actual diff, relevant source, verification manifest и raw logs. Сначала потребуй host-validated binding receipts для hashes/clean source/code tree и сверяй их IDs. Native read-only LLM не утверждает, что сама вычислила hash или запустила git; при отсутствии проверенного receipt это gap. Проверь, что actual diff был построен trusted host между заданными commits. В native read-only session получай diff/source/logs через scoped evidence inputs или ReadEvidence; не запрашивай произвольный shell и protected session roots. Если у внешнего review runner отдельно есть разрешённый Git read tool, сверяй diff им. Если bundle неполон, SHA расходятся или required tests не запускались, verdict не может быть `accept`; укажи конкретно недостающий материал.

Проверь три слоя:

1. **Mechanical:** реальные argv/exit/timeout, test inventory, failures/skips, build profile и source binding. `Done`, self-reported PASS и один exit 0 без semantic assertions не подтверждают acceptance.
2. **Behavioral:** original requirements, negative paths, invariants, replay/durability ordering, no double side effects, known/unknown classification, cancellation, ownership, queue/resource limits, privilege boundaries и сохранение чужих изменений.
3. **Integration:** actual combined tree, целостность контрактов между изменёнными и неизменёнными modules, повторные tests после integration. Green worker branch не подтверждает другой merged SHA.

Читай relevant implementation и tests. Для каждого suspected failure восстанови конкретный execution path; отделяй доказанный дефект от недоказанного риска. Новые проверки предложи VerificationRunner через requirement/test ID; самостоятельно запускай их только если отдельный review profile явно предоставляет такое право. Не меняй код, тесты, budgets, requirements, secrets или security policy. Не используй доступ исполнителя для расширения полномочий.

Выход ограничь восемью существенными findings. Для каждого: severity, requirement/invariant, точный file/symbol или lines, reproduction/trace, наблюдаемое последствие, требуемая regression и evidence status. Не включай stylistic замечания без влияния на correctness/maintainability принятого scope. Отдельно перечисли максимум пять verification gaps.

Заверши структурой: `verdict = accept | changes_requested | blocked`, reviewed base/candidate/integration SHA, фактически выполненные проверки, findings, unresolved gates. `accept` означает только review verdict; deterministic coordinator отдельно проверяет policy и evidence. Если builder/reviewer используют одну модель, явно укажи: контекст независим, модельные ошибки могут коррелировать; полностью независимого подтверждения это не создаёт.

## 3. Template продолжения в новой сессии

Продолжи проект ansa из durable repository/task state. Предыдущий ChatGPT/Codex conversation тебе не нужен и не является authority.

Цель: `{{GOAL}}`. Project root: `{{REPO_PATH}}`. Orchestration run/task: `{{RUN_ID}} / {{TASK_ID}}`. Expected base/accepted checkpoint: `{{EXPECTED_BASE_SHA}}`. Canonical task reference: `{{TASK_RECORD_REF}}`. Evidence directory: `{{EVIDENCE_REF}}`. Approved policy/budget references: `{{POLICY_REF}} / {{BUDGET_REF}}`.

Прочитай AGENTS.md, PROJECT_STATE, relevant ADR/roadmap и scoped instructions. Сверь remote branches/PR state, HEAD, dirty files, milestone checkpoint и evidence. Различай last known good, verified candidate и WIP. Markdown может устареть: task transitions восстанавливаются из canonical journal, факты о коде — из Git, результаты tests — из привязанного к SHA evidence.

Проверь ownership lock/lease, current incarnation, task/attempt IDs и отсутствие живого прежнего writer. Не запускай второй coordinator или worker только потому, что heartbeat устарел. Сначала reconciliation: supervisor/process identity, persisted started/outcome receipts, worktree contents, candidate commits и известные external side effects. Не сигнализируй процесс по одному PID без подтверждения его принадлежности.

Если outcome известен и прошёл identity/hash checks, импортируй observation штатным owner API; не переписывай journal вручную. Если состояние unknown, блокируй новый write attempt до reconciliation или явного policy decision. Resume model conversation не доказывает прекращение прежнего процесса и не заменяет новый attempt identity.

Составь краткий current-state report: verified refs, последний durable transition, pending gates, действующий budget, next permitted action. Выполняй только этот action в пределах задачи; не перескакивай migration prerequisites, не открывай реальные credentials ради диагностики и не меняй paid route автоматически. Устаревший branch после движения base требует новой integration проверки, а не молчаливого переноса старого PASS.

После действия сохраняй evidence и durable result штатными interfaces. PROJECT_STATE обновляй только проверяемыми фактами с tested SHA и следующим шагом. При недоступном prerequisite верни конкретный blocked report, сохранив WIP/artifacts и инструкцию безопасного продолжения.

## 4. Пример автоматически сформированного worker задания

Это пример **будущей узкой Rust fault-recovery задачи**, выбранной после появления соответствующего Orchestrate module. Он не утверждает, что такой дефект или interface существует сейчас. Coordinator сначала заполняет все поля:

```json
{
  "task_id": "{{TASK_ID}}",
  "parent_task_id": "{{PARENT_TASK_ID}}",
  "attempt_id": "{{ATTEMPT_ID}}",
  "owner_epoch": "{{OWNER_EPOCH}}",
  "base_sha": "{{IMMUTABLE_BASE_SHA}}",
  "worktree": "{{ALLOCATED_WORKTREE}}",
  "branch": "{{ALLOCATED_BRANCH}}",
  "requirements_ref": "{{REQUIREMENTS_REF}}",
  "allowed_paths": ["{{RECOVERY_MODULE}}", "{{RECOVERY_TEST_FILE}}"],
  "max_outer_turns": 1,
  "max_wall_seconds": 900,
  "max_repair_attempts": 0,
  "subagents": false,
  "tool_network": "deny",
  "inference_route": "{{APPROVED_INFERENCE_ROUTE}}",
  "push": false,
  "policy_ref": "{{IMMUTABLE_POLICY_REF}}"
}
```

Работай только в указанном worktree от exact base SHA. Цель: добавить regression и минимальное исправление для сценария **durable launch intent существует; worker успел изменить owned fixture; canonical outcome отсутствует после parent crash**. Требуемое поведение: recovery фиксирует неопределённость и не начинает второй write execution, пока нет валидного подтверждения завершения прежнего attempt.

Прочитай original requirements, AGENTS и actual recovery code. Не предполагай причину дефекта по этому описанию. Если current code уже удовлетворяет требованию, подтверди это regression и сообщи отсутствие необходимого production change.

Acceptance: fault fixture воспроизводит boundary без реального Codex/API; unique marker изменяется ровно один раз; повтор reopen/resume не вызывает второй dispatch; stale attempt result не продолжает текущую задачу; достоверный late outcome того же attempt сохраняется без запуска следующего действия после Cancel. Выполни переданные вместе с TaskSpec точные test commands и затронутый module gate. При выходе за allowed paths или необходимости изменить контракт верни blocker.

Нельзя менять acceptance/tests ради зелёного результата, создавать subagents, выполнять push/merge/rebase/main mutations, читать credentials, запускать сетевые tools или самостоятельно менять оплачиваемый inference route или расширять policy. Не commit-ь автоматически, если TaskSpec не предоставила такое право: coordinator снимет candidate snapshot.

Верни candidate diff и машинно читаемый результат по выданной output schema: task/attempt IDs, observed base/current tree identity, изменённые paths, фактически выполненные commands/outcomes, artifact references, unresolved blockers. Tool network denial не отключает сетевой inference самого host: этот канал предоставляет coordinator по утверждённому route. Не выводи local token/spend cap из количества outer turns. Твой текст `completed` — observation исполнителя; verification, independent review и acceptance выполняет coordinator.
