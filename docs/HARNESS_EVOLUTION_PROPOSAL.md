# Предложение: развитие harness ANSA по Hermes и Harness²

Исходный материал [PR #14](https://github.com/junknewera/ansa/pull/14). Состояние на дату предложения сохранено в [журнале этой ветки](https://github.com/junknewera/ansa/blob/ef4f4080aaa5e6c14d16f461ffdb9bc0c5dd744e/docs/PROJECT_STATE.md). Текущая реализация и результаты проверок описаны в [сводке по пяти предложениям](PROJECT_STATE.md#реализация-пяти-proposal-после-разбора-требований).

- Дата: 2026-11-15.
- Статус: **PROPOSED, docs-only**. Реализация и модельный сравнительный eval не выполнены.
- Исторический baseline предложения: [ANSA `9e6e6de`][ansa-base], после merge PR #13 с Rust SIWC streaming hotfix.
- Источники: [Harness² `6868817`][h2-base] и [Hermes `fd8a6c4`][hermes-base].
- Связанные документы: [PROJECT_STATE](PROJECT_STATE.md), [TODO](TODO.md),
  [migration plan](RUST_MIGRATION_PLAN.md), открытые [PR #3][pr3] и [PR #8][pr8].

## Уточнение состояния main — 2026-11-15

На [main `5029b8b`][ansa-main-state] M9 принят (GO); [default package][ansa-main-flake]
уже Rust, C++ sources и fallback сохранены. M10 остаётся отдельным незакрытым
этапом. Ниже сохранены наблюдения и evidence исходного `9e6e6de`: они описывают
исторический baseline этой ветки. Эта docs-only поправка не переносит в неё
код cutover и не объявляет повторную runtime-приёмку.
Согласованный порядок production implementation после verified M10 и применимых
O/LE gates сохраняется; карта технических зависимостей ниже уточняет scope этих gates.

## Решение для обсуждения

Заимствовать у Hermes управление контекстом, доступ к большим результатам tools
и загрузку навыков по запросу. У Harness² взять ограниченную изменяемую часть
harness, сравнение кандидатов и извлечение уроков с проверяемым evidence.
Реализовать эти контракты в native ANSA, сохранив собственные loop, tools,
session journal, process supervisor и direct SIWC Responses.

Ближайшая предполагаемая польза для пользователя — длинные задачи, которые
проходят несколько compact внутри одного turn, и доступ к полному сохранённому
выводу команд. Profiles и task-level eval дают основу для проверки последующих
изменений skills и памяти. Это гипотезы о полезности, а не измеренный прирост.

| Заимствование | Источник | Предлагаемый результат |
|---|---|---|
| Полный token budget | Hermes | Проверка размера request перед каждым provider call |
| Compact внутри turn | Hermes | Продолжение длинного tool loop на durable границе |
| Сохранение большого output | Hermes | Preview и ограниченное чтение по устойчивому ID |
| Skills и scoped memory | Hermes | Переиспользование процедур и фактов без загрузки всей библиотеки |
| Versioned HarnessProfile | Harness² | Точный состав инструкций, навыков и памяти каждой сессии |
| Review, candidate, eval, adoption | Оба | Проверяемое изменение следующей версии harness с допустимым no-op |

## Проверенное состояние исторического baseline

[PROJECT_STATE][ansa-state] фиксирует M8 GO и последующий целевой Rust hotfix P1.
Default package остаётся C++20; M9 live acceptance/cutover отдельно не закрыт.
Успешный ответ после P1 не заменяет полную M9-приёмку.

| Наблюдение в коде | Следствие для предложения |
|---|---|
| `agent_epoch()` сохраняет settings, полные начальные AGENTS и compaction policy в exact state [A1][ansa-epoch] | Профиль можно материализовать на существующей границе сборки контекста |
| Compact выбирается при Submit по числу предыдущих user turns, items и bytes [A2][ansa-owner] | Нужна отдельная политика для роста контекста внутри одного turn |
| Summary сохраняет последние два turns; исходный admitted result и provenance проверяются при recovery [A3][ansa-compact] | Существующую policy v1 следует сохранить при добавлении новой |
| Process runner сохраняет только ограниченные префиксы stdout/stderr и отбрасывает остаток [A4][ansa-process] | Spool нужно добавить до truncation; поздний output filter не восстановит bytes |
| `AdmittedResponse` не предоставляет typed usage [A5][ansa-response] | Учёт реальных provider tokens требует отдельного контракта |
| ToolRegistry разделяет descriptor, revision, validator и policy [A6][ansa-registry] | Read-only skill/output loaders добавляются как compiled tools |
| Continuation связан с исходными request, epoch, tools и outcomes [A7][ansa-replay] | Replay старой трассы не измеряет качество нового prompt |

Journal, full-batch admission, explicit Resume, `effect_unknown`, scoped AGENTS,
plan mode и cancellation уже существуют. Новые возможности используют эти
контракты. Изменение конфигурации текущей сессии через mutable файлы запрещено
существующим exact-context binding.

## Совместимость с принятыми границами проекта

Открытый [ADR 0002][adr2] фиксирует обязательный native-only product decision;
[ADR 0003][adr3] развивает его для диагностической эскалации. Их исторические
baseline не заменяют текущий snapshot этого предложения. Следующие ограничения
сохраняются:

1. Model calls solver, reviewer и optimizer идут через native ANSA/direct SIWC.
   Внешний eval runner управляет fixtures и ANSA CLI; он не запускает Hermes,
   Codex SDK/app-server, OpenCode или другой agent executor. Недоступный route,
   account/model/effort не заменяется скрытым fallback или paid API.
2. В MVP один active model child и один writer. Review сначала запускается
   последовательно в отдельной session. Рекурсивная делегация и параллельные
   reviewers требуют следующего lifecycle/auth gate.
3. Session journal остаётся authority для исполнения. Versioned eval/learning
   journal отвечает за решения эксперимента и adoption, ссылаясь на session
   receipts. Новые task events не вставляются в strict legacy session v1.
4. Auth, permissions, budgets, evaluator, durable ordering и классификация
   эффектов принадлежат host. Guidance и skills не могут менять эти полномочия.
5. Root budgets включают retries, Resume, compact, review и verification;
   переход в новую session и смена роли не обнуляют их и не добавляют allowance.
6. Default production features следуют verified M10 GO и соответствующим O/LE
   gates. HE0 — текущий design proposal; более ранний изолированный spike требует
   отдельного решения. M0–M10 и существующая очередность не расширяются этим PR.

## 1. Versioned HarnessProfile и evidence export

У Harness² `SurfaceSpec` задаёт допустимые paths, размер always-on content и
правила компонентов [H1][h2-surface]. ANSA нужна более узкая изменяемая поверхность.

Предлагаемый `HarnessProfile` содержит ID/revision, parent revision, digest,
текст workflow/tool guidance, индекс выбранных skills, memory snapshot и ID
зарегистрированной context policy. Это новое понятие, не существующий API.
Account/model binding и ToolSnapshot фиксируются рядом как часть run identity,
но optimizer не получает права их менять.

Для MVP разрешены только правки явно выделенного guidance и кандидаты
процедурных skills. Пользовательские AGENTS, принятые project requirements,
security policy, credentials, budgets, evaluator и compiled implementation
не входят в editable surface. Валидатор проверяет paths, scope, schema и
размеры; невалидный candidate не применяется. Он не доказывает истинность текста.

Точка интеграции — `agent_epoch()` в `src/agent_config.cpp` и
`rust/crates/ansa/src/config/agents.rs`. В snapshot фиксируются фактически
загруженные bytes/revisions и их digest. Путь на mutable файл сам по себе не
является identity. В MVP новая revision применяется к новой session; Resume
использует свой initial profile и текущую epoch, восстановленную reducer.

Evidence export — производное read-only представление journal: задача,
идентичности request/operation, вызовы, известные outcomes, артефакты, итог и
причина остановки. Timeout, Cancel, storage failure и `effect_unknown` различаются;
они не переименовываются в добровольное завершение агента. Export содержит
достаточные ссылки на исходные записи, но не credentials и не текстовую
интерпретацию чужого encrypted reasoning. Индекс и export можно перестроить.

**Приёмка:** изменение skill по прежнему пути не меняет Resume; несовпадение
profile/tool/model/input identity отклоняется; слишком большой или выходящий
за allowlist candidate не активируется; потерянный индекс восстанавливается
из authority без provider/tool dispatch.

## 2. Token-aware budget и новая compact policy

Hermes оценивает system prompt, messages и tool schemas вместе [H2][hermes-tokens].
Сохраняемый хвост учитывает token budget и целые tool-call/result groups
[H3][hermes-tail]. Конкретные коэффициенты и пороги Hermes не принимаются как
универсальные настройки ANSA.

### Учёт перед каждым вызовом модели

- Сохранить byte/item limits как ограничения памяти и протокола.
- Добавить typed optional usage с provenance из admitted provider response;
  отсутствие usage остаётся `unknown`, а не нулём. Учёт не ослабляет admission.
- Оценивать весь подготовленный request перед generation, continuation,
  summarization и review. Учитывать окно проверенной модели и резерв на ответ;
  неизвестное окно требует явной проверенной policy, а не угаданного unlimited.
- Различать approximate estimate и reported usage; не смешивать total input,
  cached input и output при подсчёте. После compact старый usage baseline
  переоценивается для новой epoch.
- Калибровать estimator на русском тексте, коде и tool logs. `chars / 4`
  без проверки языка не является достаточным контрактом.
- Ограничить число compact и минимальную полезную экономию, чтобы обслуживание
  контекста не занимало весь turn. Лимит токенов не обещает точный in-flight
  SIWC credit cap; уже отправленный запрос может расходовать ресурсы до отмены.

### Compact между tool rounds

Новая policy работает только на границе перед следующим provider request,
после полного batch admission и сохранения всех известных outcomes завершённого
batch. Pending intent, неизвестный эффект или неподтверждённый cleanup блокируют
этот переход. Compact не скрывает recovery blockers и не повторяет инструменты.

Преобразование сохраняет исходный user goal, действующие ограничения, незавершённые
действия, активные instruction scopes и ссылки на важные результаты. Retained
tool-call/result groups остаются целыми; нельзя заменить output в старом outcome.
Retained provider-native items сохраняют порядок, полные значения, call/result IDs
и opaque/encrypted fields без перезаписи или интерпретации. Исключение items
допускается только новой versioned compact transformation с сохранённым исходным
request и provenance; прежние outcomes и journal records не меняются.
Новая assembled context epoch и provenance синхронизируются до её использования.
Cancel и failed persistence не публикуют неподтверждённый новый контекст.

`prompt_summary_keep_two_v1` и `omh.summary.v1` остаются читаемыми с прежними
правилами. Новый prompt/codec получает отдельную versioned policy. Совместимость
новых состояний со старым reader не объявляется без проверки; unsupported state
останавливает dispatch, без перезаписи или отката journal к старой копии.

**Приёмка:** один длинный turn проходит несколько compact, сохраняя constraints
и tool dependencies; restart продолжает из сохранённой epoch; crash/Cancel до
и после compact commit не теряет outcomes; невозможно получить бесконечный
compact loop или обойти total budget. Качество проверяется задачами с точными
контрольными фактами, а не только наличием summary.

## 3. Сохраняемый output и bounded readback

Hermes возвращает preview и адрес сохранённого oversized result, объясняя, что
для восстановления вывода следует читать архив, а не повторять remote operation
[H4][hermes-output]. В ANSA это особенно полезно для build/test logs.

Spool добавляется в process runner до отбрасывания bytes. Отдельно ограничиваются
inline preview, размер stdout/stderr blobs и общий дисковый бюджет session.
Полный output гарантируется только в пределах установленной spool quota;
при её исчерпании результат явно сообщает неполноту. Чтение pipe и Cancel
остаются bounded и не ждут бесконтрольного дискового I/O.

Предлагаемые поля результата: `output_id`, digest, сохранённые и наблюдавшиеся
размеры, completeness/truncation, preview, exit/timeout/cancel/cleanup status.
Предлагаемый read-only tool `read_output(id, offset, limit)` проверяет принадлежность
session, identity и bounds. Произвольный путь не даёт доступ к credential stores.

Порядок публикации: завершить и надёжно сохранить blob → связать его identity с
supervisor evidence → сохранить journal outcome/reference. Crash до journal
append допускает reconciliation по той же operation identity. Hash без доступного
blob не считается успешным readback. Потеря output не превращает известный эффект
команды в `not_started` и не разрешает повторный запуск ради восстановления лога.

GC учитывает durable references и незавершённые операции. Нельзя удалять blob,
пока на него ссылается сохраняемая session. Tool implementation revision и
process-evidence reader/writer contract обновляются явно; старые outcomes читаются
в прежнем формате.

**Приёмка:** большая ошибка в конце лога доступна через readback; чтение не
вызывает повтор команды; quota/disk-full не блокируют Cancel; crash между blob,
evidence и outcome проверен; missing/corrupt blob даёт диагностику, а не выдуманный
полный output. Fixtures проверяют восстановление и ссылки после restart.

## 4. Skills, scoped memory и поиск по сессиям

Hermes разделяет небольшой frozen memory snapshot и live store [H5][hermes-memory],
а skills раскрывает по запросу [H6][hermes-skills]. Для ANSA предлагаются четыре
разные функции:

| Слой | Содержимое | Authority и применение |
|---|---|---|
| Session journal | Факты исполнения и native context | Recovery исходной session |
| Summary | Рабочее состояние текущей задачи | Versioned context transformation |
| Memory | Отобранные устойчивые факты с scope | Immutable snapshot для новой session |
| Skills | Процедуры, условия применения и проверки | Версионированное чтение по запросу |

Первый этап — curated read-only catalog с именем, кратким trigger, revision и
digest; условные API `list_skills` и `read_skill`. Тело и references загружаются
ограниченно, их точные bytes/revision попадают в сохраняемый tool result/context.
Scope и priority не позволяют learned skill переопределить пользовательские
инструкции. Не загружается executable tool/plugin из текста skill.

После compact loader различает «skill был прочитан» и «тело skill ещё доступно
в активном контексте». Если содержимое исключено, нужен marker и повторное чтение
той же закреплённой revision; в Hermes это выражено через `SKILL_PRUNED`
[H7][hermes-reload]. Удалённая revision не заменяется молча новой.

Memory начинается с небольшого project-scoped набора фактов с источником,
revision и условиями применимости. Global/user scope включается явно.
Предпочтение не выводится из случайного one-off запроса. Размер и число попыток
обслуживания памяти ограничены; overflow не даёт молча удалить прежние записи.

Для эпизодов нужен ограниченный поиск исходных сообщений и selected outcomes
с устойчивыми anchors. Индекс производен от разрешённых journal данных, соблюдает
session/project/account scope и не становится вторым источником исполнения.
FTS или другой индекс выбирается по измеренному случаю; новая vector DB не является
prerequisite. Утрата индекса не мешает recovery.

**Приёмка:** библиотека не загружается целиком в каждый request; plan не получает
write tools; Resume сохраняет старые revisions; reload после compact возвращает
нужный текст; одноимённые skills и scope-конфликты разрешаются детерминированно;
поиск возвращает исходные сообщения с bounds и проверяемыми ссылками.

## 5. Review → candidate → eval → adoption

У Hermes review fork отделён от persistence родительского разговора, ограничен
dispatch whitelist и суммарным input budget [H8][hermes-review]. В ANSA reviewer
создаётся как отдельная native session, сначала последовательно. Его внутренние
prompts/summary не импортируются в пользовательскую историю как standing instructions.
Auth/lease lifecycle остаётся у native subsystem.

Reviewer сначала имеет чтение evidence и возможность предложить patch. Он не
меняет активный profile, runtime, permissions, budgets или evaluator. Перед patch
фиксируется свежая preimage revision. Все предложения — untrusted model output,
который host проверяет на schema, scope, identity и допустимые размеры.

Урок хранит trigger/cue, конкретное изменение, source session/operation IDs,
parent revision, evidence, verdict и validation status. Применимы классификации
`helped`, `hurt`, `no observed effect`, `confounded`. Один удачный rollout или
победивший deliverable не устанавливает причинный эффект правки.
Harness² допускает `NO_ENTRY`, если переносимый урок не подтверждён
[H9][h2-lessons]. Этот no-op является нормальным результатом review.

Не переносится стимул обновлять skill почти после каждой session из Hermes
[H10][hermes-review-pressure]. Временная ошибка окружения не превращается в
постоянный запрет инструмента, неудачные попытки — в проверенный рецепт.

### Постоянная запись и recovery

Skill/memory store получает одного владельца записи и versioned mutation records.
Before/after snapshots, actor и evidence полезны, но best-effort skill ledger
Hermes не подходит для гарантии принятия изменения: он допускает mutation после
ошибки записи ledger [H11][hermes-ledger].

Session effectful tool использует обычные durable intent/outcome и стабильный
mutation ID. Store transaction фиксирует preimage, staged blobs, candidate revision
и проверяемый receipt. До durable adoption candidate не выбирается по умолчанию
для новых sessions. Active revision определяется только committed adoption в
learning journal; её указатель в store является восстанавливаемой проекцией.
Crash между store receipt и adoption не активирует candidate. Learning journal
принимает adoption только после verified evaluation и связывает тот же ID/revision.
Между journal domains нет общей
транзакции: recovery сначала reconciles прежний mutation/receipt и pending decision,
не повторяет изменение слепо. Missing evidence, конфликт revision и неизвестный
эффект блокируют adoption. Материализованные indexes перестраиваются из authority.

Receipt import и завершённое evaluation/adoption имеют разные dedup guards.
Crash после import, но до final decision, должен завершить pending transition;
новый experiment не запускается лишь потому, что receipt больше не новый.
Rollback активирует новую revision со ссылкой на прежнее содержимое; он не
откатывает историю исполнения и не меняет snapshots открытых sessions.

**Приёмка:** слабый evidence даёт `NO_ENTRY`; stale patch отклоняется; crash/double
delivery не создают повторную mutation; failed persistence не объявляется adoption;
отмена reviewer сохраняет известные outcomes, но не запускает новые действия.
Автоматизация включается только после измерения пользы ручного propose/eval flow.

## 6. Task-level eval перед полной рекурсией Harness²

Eval driver создаёт отдельные ANSA sessions и resettable fixtures для baseline
и одного candidate. Запускается тот же native route с одинаковыми model/effort,
policy и budgets. Начинать с одной небольшой правки; несколько одновременно
изменённых компонентов требуют ablation или verdict `confounded`.

Run identity включает source/build, profile, task prompt и relevant file bytes,
environment, account binding hash, model/effort, tool implementation revisions,
context policy и frozen evaluator. Digest предназначен для проверки identity;
он не делает стохастические model executions детерминированными. Новый target
делает прежний verdict stale. Baseline всегда остаётся допустимым выбором.

До trials фиксируются критерии успеха, допустимые регрессии, бюджеты и holdout
задачи. Входные tasks/fixtures доступны solver; эталонные ответы, grading rules
и holdout outcomes ему не передаются. Trial runs не пополняют active memory и
skills в обход candidate/adoption. Full-access shell/worktree не считается
network/filesystem sandbox: для скрытых graders нужна действительно enforced
изоляция либо evaluator вне доступной solver среды; без неё результат помечается
как потенциально загрязнённый и не доказывает переносимость.

| Метрика | Правило |
|---|---|
| Успех задачи | Проверяемый artifact/condition и frozen host evaluator; `Done`/exit 0 недостаточно |
| Регрессии | Потерянные требования и нарушения safety/recovery gates — отдельный отказ |
| Расход | Calls, reported/estimated tokens раздельно, wall time, tools, review/compact/retries |
| Неполные попытки | Infra/timeout/cancel учитываются явно, failures не удаляются из отчёта |
| Переносимость | Задачи, по которым candidate не настраивался, и повторные trials при неоднозначности |

SIWC plan usage не переводится в API dollars по публичному прайсу. Если usage или
allowance неизвестны, соответствующее поле остаётся unknown и сравнение не
называется equal-cost. Счётчики всех phases переживают Resume/restart и новые
sessions, включая reserve на final verification; лимиты O/LE не суммируются
с отдельным «бесплатным» review budget.

Прежний replay проверяет восстановление исходного выполнения. Для оценки нового
guidance нужны новые model runs; cached responses не выдаются за эксперимент.
Probes выполняются на owned resettable data. Повтор реального внешнего side effect
ради A/B запрещён; lost ACK сверяется по прежней session/operation identity.

### Простые сравнения для каждого компонента

| Проверяемая гипотеза | Контроль перед усложнением |
|---|---|
| Summary помогает длинным задачам | Сравнить с маскированием старых observations и с сохранением хвоста плюс retrieval исходных сообщений при одинаковых задачах, model/effort и доступных исходных данных; все варианты сохраняют описанные native-item/tool-pair/epoch контракты |
| Lazy skills полезнее постоянных инструкций | Сравнить статическую загрузку и загрузку по запросу одного и того же набора skills с одинаковыми bytes/revisions; учитывать стоимость каталога, чтения и reload |
| PPC окупает дополнительные прогоны | Сначала baseline/no-op и простой propose/eval одного candidate; затем PPC при общем budget, с тем же frozen evaluator и независимым holdout |

Ограничения на расход и final acceptance фиксируются одинаково для сравниваемых
arms; фактические calls, tokens и latency сохраняются раздельно. Порог пользы
выбирается до trials. Превосходство над no-op само по себе не доказывает пользу
PPC относительно простого propose/eval.

### Что адаптировать в Harness²

- Его полный `k=1` — **4 task rollouts + 3 improver sessions**, reflection/retries
  отдельно [H12][h2-cost]; при одном final selector session не нужен
  [H12a][h2-single-selection].
- `selection_pool()` исключает исходный baseline [H13][h2-selection]. ANSA сохраняет
  baseline и явное решение reject/no change. Нельзя обещать монотонное улучшение.
- `Substrate` отделяет запуск от метода оптимизации [H14][h2-substrate]. Заимствуется
  интерфейс driver/materialisation/evidence; штатные внешние executors Harness²
  не становятся runtime dependencies ANSA.
- Автоматический Propose–Probe–Compose добавляется только после полезного простого
  A/B и ограничивается host budget. Он не запускается для каждого пользовательского
  Submit по умолчанию.

**Приёмка:** идентичности и evaluator проверяются до trials; baseline может победить;
failed/inconclusive runs сохранены; holdout не участвует в настройке; неизвестные
cost fields не подменены нулями; crash не обнуляет budget и не повторяет effects.

## Orchestrate и отложенные возможности

Этот proposal использует native child lifecycle из [PR #3][pr3] и диагностические
границы [PR #8][pr8], не вводит второй controller. Сначала read-only child/reviewer
с небольшой структурированной выдачей; writers получают отдельные owned worktrees,
parent/child identities, budgets, cancel/join ownership и проверяемые receipts.

Hermes worktree helper при ошибке допускает fallback `None` [H15][hermes-worktree].
Для согласованного Orchestrate MVP отсутствие изоляции — явный blocker. Вне Git
Orchestrate остаётся blocked согласно ADR 0002; обычные basic/plan доступны.
Non-Git orchestration требует отдельного будущего решения.

MCP/dynamic executable tools, lazy tool schema catalog, автоматическая установка
plugins, удалённые execution backends, большой memory framework и параллельный
recursive optimizer остаются вне первого scope. Skills MVP загружает документы;
новый tool snapshot lifecycle проектируется отдельно вместе с MCP.

## Технические зависимости компонентов

| Компонент | Необходимые контракты |
|---|---|
| Manifest/profile и evidence export | Существующие session/turn/request identities, exact epoch и read-only проекция session journal; manifest/profile binding добавляется как новый контракт, native child lifecycle для самого экспорта не требуется |
| Usage и request accounting | Production request builder, admitted response с typed optional usage, сохраняемые бюджеты и provenance |
| Mid-turn compact | Полный request accounting, versioned context policy и durable граница после известных outcomes |
| Output spool/readback | Process runner, bounded storage, ссылки из supervisor evidence/journal, quota и recovery |
| Curated skills, session search и memory snapshots | Read-only loaders, pinned revisions, scope и journal-derived index; optimizer для их чтения не требуется |
| Review и learning/adoption | Дополнительно native child lifecycle, frozen evaluator, staged candidate и durable adoption/reconciliation; здесь применимы соответствующие O/LE контракты |
| Propose–Probe–Compose | Проверенный простой propose/eval и измеренное преимущество над ним при общем budget |

Эта карта определяет технический scope каждого компонента. Согласованная
очередность M/O/LE/HE сохраняется отдельно: production implementation начинается
после verified M10 и применимых gates, а более ранняя реализация или pilot
требуют отдельного решения. Наличие независимого компонента не закрывает чужие gates.

## Предлагаемая последовательность

HE обозначает будущие feature gates и не заменяет migration M, Orchestrate O или
loop-engineering LE. Production implementation следует verified M10 и применимым
O/LE prerequisites; данный PR не назначает live pilot и не включает feature.

| Этап | Scope | Проверяемый выход |
|---|---|---|
| HE0 | Этот proposal | Source-backed contracts, dependencies, independent design review |
| HE1 | Profiles, evidence export, offline eval driver | Exact identity, baseline/no-op, synthetic failure/recovery cases |
| HE2 | Typed usage и token budget | Полный request accounting, bounds и unknown usage semantics |
| HE3 | Новая mid-turn compact policy | Long-turn quality, crash/Cancel, exact epoch и legacy reads |
| HE4 | Bounded output spool/readback | Durable reference, quota, GC/recovery и отсутствие повторного exec |
| HE5 | Read-only skills, memory snapshots, session search | Lazy load, scope, pinned revisions и reload после compact |
| HE6 | Proposal/mutation/adoption lifecycle | Review isolation, durable reconciliation, holdout и rollback revision |
| HE7 | Optional Propose–Probe–Compose | Измеренная польза относительно простого A/B при общем budget |

HE2–HE4 — приоритет по непосредственной пользовательской пользе. HE1 обеспечивает
проверяемость новых profiles и learning; curated skills не требуют HE7.
Внутри HE1 можно отдельно проверить manifest/identity, read-only export и offline
baseline/candidate driver. Внутри HE5 — curated read-only catalog, session search
и scoped memory snapshots. Эти части получают отдельные минимальные diffs;
номера HE1/HE5 и полные критерии выхода соответствующих gates сохраняются.
Заимствование child machinery не меняет порядок O1–O4. Каждый этап получает
собственный минимальный diff и acceptance; увеличение scope требует нового решения.

## Проверка документа и открытые вопросы

Acceptance cases выше **специфицированы для будущей реализации**, а не объявлены
выполненными tests. В этом PR проверяются Markdown/links, фактический baseline,
совместимость с существующими ADR, область diff и independent design review.
Результаты фиксируются в PR body. Runtime/native/live acceptance не следует из
успешной проверки документа.

До реализации открыты: доступность usage/context-window metadata в выбранном SIWC
route; новый compact codec; конкретная spool quota и retention; single-writer
store/reconciliation API; tool-result references и проверка их доступа; качество
scoped retrieval; состав task fixtures, evaluator и критерий пользы. Численные
budgets выбираются до pilot, затем пересматриваются по evidence.

Core Harness² распространяется под [Apache-2.0][h2-license], Hermes — под
[MIT][hermes-license]. При прямом переносе кода сохраняются применимые notices,
а third-party компоненты проверяются отдельно. Этот proposal переносит идеи и
контракты; upstream runtime/code dependencies не добавляются.

## Первоисточники

Ссылки на код закреплены на commits. Описанные upstream механизмы прочитаны в
исходниках; их runtime guarantees и benchmark gains не считаются измеренными
свойствами ANSA.

[ansa-base]: https://github.com/junknewera/ansa/tree/9e6e6dec451e4b3f8c6c72481b3df1e5f8a10e88
[ansa-state]: https://github.com/junknewera/ansa/blob/9e6e6dec451e4b3f8c6c72481b3df1e5f8a10e88/docs/PROJECT_STATE.md
[ansa-main-state]: https://github.com/junknewera/ansa/blob/5029b8b8972ea4c64ac213474b37f53a13bd5ab5/docs/PROJECT_STATE.md
[ansa-main-flake]: https://github.com/junknewera/ansa/blob/5029b8b8972ea4c64ac213474b37f53a13bd5ab5/flake.nix
[ansa-epoch]: https://github.com/junknewera/ansa/blob/9e6e6dec451e4b3f8c6c72481b3df1e5f8a10e88/rust/crates/ansa/src/config/agents.rs
[ansa-owner]: https://github.com/junknewera/ansa/blob/9e6e6dec451e4b3f8c6c72481b3df1e5f8a10e88/rust/crates/ansa/src/runtime/owner.rs
[ansa-compact]: https://github.com/junknewera/ansa/blob/9e6e6dec451e4b3f8c6c72481b3df1e5f8a10e88/rust/crates/ansa/src/context/legacy_compact.rs
[ansa-process]: https://github.com/junknewera/ansa/blob/9e6e6dec451e4b3f8c6c72481b3df1e5f8a10e88/src/process.cpp
[ansa-response]: https://github.com/junknewera/ansa/blob/9e6e6dec451e4b3f8c6c72481b3df1e5f8a10e88/rust/crates/ansa/src/provider/responses.rs
[ansa-registry]: https://github.com/junknewera/ansa/blob/9e6e6dec451e4b3f8c6c72481b3df1e5f8a10e88/rust/crates/ansa/src/tools/registry.rs
[ansa-replay]: https://github.com/junknewera/ansa/blob/9e6e6dec451e4b3f8c6c72481b3df1e5f8a10e88/rust/crates/ansa/src/provider/replay.rs
[pr3]: https://github.com/junknewera/ansa/pull/3
[pr8]: https://github.com/junknewera/ansa/pull/8
[adr2]: https://github.com/junknewera/ansa/blob/cac4f12c8063f54c032bbd532f59e047a59891f2/docs/adr/0002-self-hosted-orchestration.md
[adr3]: https://github.com/junknewera/ansa/blob/6f6d09c5ab21db761ca9abdf47b1e9cbfbc73163/docs/adr/0003-gpt-diagnostic-escalation.md
[h2-base]: https://github.com/google-research/harness2/tree/6868817518876de8fd170c6a8354daca273b6191
[hermes-base]: https://github.com/NousResearch/hermes-agent/tree/fd8a6c41ace59b476193c61002ce6a86d667b18a
[h2-surface]: https://github.com/google-research/harness2/blob/6868817518876de8fd170c6a8354daca273b6191/harness2/core/tree.py#L41-L149
[hermes-tokens]: https://github.com/NousResearch/hermes-agent/blob/fd8a6c41ace59b476193c61002ce6a86d667b18a/agent/model_metadata.py#L2553-L2565
[hermes-tail]: https://github.com/NousResearch/hermes-agent/blob/fd8a6c41ace59b476193c61002ce6a86d667b18a/agent/context_compressor.py#L4796-L4845
[hermes-output]: https://github.com/NousResearch/hermes-agent/blob/fd8a6c41ace59b476193c61002ce6a86d667b18a/tools/tool_result_storage.py#L393-L464
[hermes-memory]: https://github.com/NousResearch/hermes-agent/blob/fd8a6c41ace59b476193c61002ce6a86d667b18a/tools/memory_tool_store.py#L177-L258
[hermes-skills]: https://github.com/NousResearch/hermes-agent/blob/fd8a6c41ace59b476193c61002ce6a86d667b18a/tools/skills_tool.py
[hermes-reload]: https://github.com/NousResearch/hermes-agent/blob/fd8a6c41ace59b476193c61002ce6a86d667b18a/agent/context_compressor_tail.py#L71-L104
[hermes-review]: https://github.com/NousResearch/hermes-agent/blob/fd8a6c41ace59b476193c61002ce6a86d667b18a/agent/background_review.py#L1055-L1219
[h2-lessons]: https://github.com/google-research/harness2/blob/6868817518876de8fd170c6a8354daca273b6191/harness2/core/prompts.py#L1105-L1169
[hermes-review-pressure]: https://github.com/NousResearch/hermes-agent/blob/fd8a6c41ace59b476193c61002ce6a86d667b18a/agent/background_review.py#L510-L535
[hermes-ledger]: https://github.com/NousResearch/hermes-agent/blob/fd8a6c41ace59b476193c61002ce6a86d667b18a/tools/skill_ledger.py#L298-L323
[h2-cost]: https://github.com/google-research/harness2/blob/6868817518876de8fd170c6a8354daca273b6191/harness2/core/ledger.py#L15-L33
[h2-single-selection]: https://github.com/google-research/harness2/blob/6868817518876de8fd170c6a8354daca273b6191/harness2/core/selection.py#L360-L398
[h2-selection]: https://github.com/google-research/harness2/blob/6868817518876de8fd170c6a8354daca273b6191/harness2/core/selection.py#L93-L111
[h2-substrate]: https://github.com/google-research/harness2/blob/6868817518876de8fd170c6a8354daca273b6191/harness2/substrate/base.py#L27-L87
[hermes-worktree]: https://github.com/NousResearch/hermes-agent/blob/fd8a6c41ace59b476193c61002ce6a86d667b18a/tools/subagent_worktree.py#L77-L107
[h2-license]: https://github.com/google-research/harness2/blob/6868817518876de8fd170c6a8354daca273b6191/LICENSE
[hermes-license]: https://github.com/NousResearch/hermes-agent/blob/fd8a6c41ace59b476193c61002ce6a86d667b18a/LICENSE
