# Эволюция промптов агентных harness: исходники, причины и evidence

Срез публичных исходников и GitHub metadata: **2026-11-15**. Исследование для
ANSA на baseline [a6f2031](https://github.com/junknewera/ansa/commit/a6f203173969c72f9b9e9b0a8ad873958215db6d).
Это исследовательский proposal: runtime, production prompts, journal codecs и
проверенные milestones этим документом не изменяются. Upstream tests и model
experiments ниже описаны по источникам; в рамках исследования они не запускались.

## 1. Ответ на вопрос

**Причины изменений часто можно восстановить, но одного git log недостаточно.**
Полезная цепочка — исходный prompt и место его подстановки → финальный diff →
commit metadata → PR body → review/issue → конкретный тест или эксперимент.
Встречаются и подробно разобранные регрессии, и изменения с пустым объяснением.
Одобрение PR, наличие теста на строку и уменьшение числа токенов отвечают на
разные вопросы.

Наиболее повторяющиеся причины в изученной выборке:

- инструкции расходятся с реальными tools, mode или текущей задачей;
- разные слои prompt противоречат друг другу;
- compaction теряет статус запроса, проверки или гипотезы, либо копит всё подряд;
- динамические данные меняют стабильный prefix, а replay переписывает историю;
- модель уже умеет часть поведения, которое раньше подробно объясняли;
- UI/parser/protocol требуют другой формы ответа.

**Для ANSA полезнее перенять проверяемые контракты и regression cases, чем
заменять её короткие prompts большими чужими шаблонами.** При этом обнаружен
конкретный compatibility риск: текущие plan и compact тексты участвуют в
восстановлении сохранённых запросов. Их простая замена способна отклонить старый
journal. Обоснование и условия безопасного изменения — в разделе 10.

### Что означает «начальный промпт»

Здесь это не первый пользовательский запрос. Это effective initial request:
base/model instructions, project instructions, mode, environment, tool
descriptions и дополнительные блоки, которые loader реально отправляет модели.
Отдельно прослежено раннее публичное состояние там, где оно доступно. Первый
видимый import не считается первым внутренним экспериментом проекта.

## 2. Источники, версии и метод

| Проект | Зафиксированный source snapshot | Что именно исследовано |
|---|---|---|
| OpenCode | [anomalyco/opencode@d432a06](https://github.com/anomalyco/opencode/tree/d432a06b9c07bab0be4e8d642173b1cb14137f11), dev | Current system/model plugins, plan reminders, app-owned compact и hooks |
| Kimi CLI legacy | [MoonshotAI/kimi-cli@9ab1286](https://github.com/MoonshotAI/kimi-cli/tree/9ab1286b8fe4e6bcd116949a27ce5e0ac3389c82), main | Python loader, system, plan, compact и переходы путей |
| Kimi Code | [MoonshotAI/kimi-code@f082f04](https://github.com/MoonshotAI/kimi-code/tree/f082f04a093a3e71a2263c5461d52507dc31f83a), main | Открытый agent-core-v2, profile composition, plan и compaction injections |
| Codex | [openai/codex@5cb38e7](https://github.com/openai/codex/tree/5cb38e79842a0e0a6dca5bb97874163e8ee50129) | Rust prompts/catalog resolution, collaboration modes, local/remote compact, replay |
| Pi | [earendil-works/pi@7ce13c0](https://github.com/earendil-works/pi/tree/7ce13c086831838c27aeabf2e7b38d1d825d6ac9) | Coding-agent CLI и отдельно P2/durable, plan example, compact/branch summaries |
| Harness² | [google-research/harness2@6868817](https://github.com/google-research/harness2/tree/6868817518876de8fd170c6a8354daca273b6191), main | Meta-optimizer phase prompts, baseline overlays, retry и audit artifacts |
| DeepSeek Harness | [deepseek-ai/deepseek-harness@39163dd](https://github.com/deepseek-ai/deepseek-harness/tree/39163dd30f703a79079e2d2446545cff3c453738), master | System sections/persona, deployment plan policy, compaction text/route/cache, decision notes |
| Hermes | [NousResearch/hermes-agent@5e9c0cd](https://github.com/NousResearch/hermes-agent/tree/5e9c0cd31e783605ada6bd6f01da505bf9b6b10b), main | Дополнительный bounded case: prompt builder, /plan dispatch, compact, prefix cost |

Kimi CLI [архивирован](https://github.com/MoonshotAI/kimi-cli); его собственное
описание указывает на Kimi Code. Исходники преемника доступны, поэтому вывод о
«закрытом native runtime без prompt source» был бы неверным. Старый endpoint
badlogic/pi-mono перенаправляет на тот же repository ID 1035029907, что
earendil-works/pi. Одноимённые старые OpenCode проекты в эту выборку не включены.

[Машиночитаемый evidence ledger](research/prompt-evolution-evidence.json)
содержит полные SHA, parents, доступные author/committer/PR timestamps, релевантные
пути, источники, coverage и ограничения. Даты здесь приведены по UTC; точные
author и committer даты в ledger могут различаться. У Pi часть хронологии
привязана к author-date; дата создания PR отдельно не означает дату внедрения.

### Как оценивалась убедительность

| Вид свидетельства | Что позволяет утверждать | Чего не доказывает |
|---|---|---|
| Source/diff | Текст, роль, порядок, условие включения или guard действительно изменены | Модель стала лучше выполнять реальные задачи |
| Documented rationale | Автор/reviewer связал изменение с конкретной проблемой | Эта проблема была единственной причиной или исправлена на всех моделях |
| Targeted regression | Проверяется воспроизведённый structural failure, parser или request shape | Общая fidelity summary и отсутствие других behavioral regressions |
| Bounded model comparison | Есть наблюдаемый результат на описанной выборке | Перенос на другую модель, бюджет, dataset или продукт |
| Missing/inconclusive | Причина неизвестна либо эксперимент не различил варианты | Ни положительный, ни отрицательный общий эффект |

Изучались не только файлы с именами prompt/system/compact, но и builders,
dispatch, tool declarations, replay, renames и связанные discussions.
Runtime harnesses и модели не запускались; credentials не требовались.
Это история выбранных значимых изменений, а не полный аудит всех веток,
релизов, закрытых evaluations и серверных инструкций.

## 3. Где реально живут initial, plan и compact

| Проект | Initial/system | Plan | Compact и прочие инструкции |
|---|---|---|---|
| OpenCode | Base renderer зависит от объявленных tools; optimize plugin выбирает model-specific template; custom agent.system имеет отдельный путь | Durable agent state плюс enter/leave reminder и reconcile; edit guard ограничивает запись вне plan directory | App-owned structured create/update summary; retained tail; provider-native path отдельно. Explore, title и PR-style summary — другие prompt families |
| Kimi CLI → Code | Legacy Jinja+YAML; current profile builder собирает base, coding/delivery, role/tool/plugin/project/environment sections | Full/sparse/reentry/exit injections, review и tool guards; текущий root workflow отличается от subagent | Legacy XML-подобный checklist; current task-shaped handoff, latest-user anchor, live TODO и journal recovery pointer |
| Codex | Model catalog/config override + compiled templates + world-state fragments: mode, permissions, AGENTS, environment | Developer-controlled collaboration mode; update_plan — отдельный checklist tool | Local summary template и remote V2 protocol различны; review rubric тоже отдельная задача |
| Pi CLI | Named system sections + active tool guidelines + project context/skills/cwd; extensions могут менять request | Optional extension example, а не единый обязательный core mode | Initial/update/split-turn compact и branch summary различны. P2 использует другой app loader и durable compact |
| DeepSeek Harness | Ordered section registry, persona prefix/suffix/complete, tool/plugin sections; runtime facts отдельно | Deployment-owned plan:policy; logged state и review; sandbox/approval независимы | Summary directive после оригинального prefix, configurable routing, structured checkpoint, continuation wrapper |
| Harness² | Prompt optimizer поверх executor harness; baseline может быть только additive overlay | Propose/aggregate/compose/select — фазы оптимизации, не пользовательский Plan Mode | Experience prompt пишет evidence-scoped lessons; обычный compact принадлежит underlying executor |
| Hermes | Stable → context → volatile builder, model/tool/platform guidance, project context и plugins | /plan добавляет обычный planning turn; проверенный CLI dispatch не переключает tool policy | First/iterative summaries и explicit uncertainty section; rebuild system на compact boundary |

Ни одна ячейка не означает, что весь production prompt восстановим по одному
файлу. Например, Codex может получить model instructions из remote/cached
catalog, а provider-native compaction не раскрывает серверный summarizer prompt.

## 4. OpenCode: минимизация, восстановление mode и контроль роста summary

Текущий [model-request.ts](https://github.com/anomalyco/opencode/blob/d432a06b9c07bab0be4e8d642173b1cb14137f11/packages/core/src/session/model-request.ts)
выбирает custom agent.system либо
[tool-conditioned renderer](https://github.com/anomalyco/opencode/blob/d432a06b9c07bab0be4e8d642173b1cb14137f11/packages/core/src/session/system-prompt.ts),
после чего добавляются instructions.
[optimize.ts](https://github.com/anomalyco/opencode/blob/d432a06b9c07bab0be4e8d642173b1cb14137f11/packages/core/src/plugin/optimize.ts)
подставляет разные GPT/Astra/Anthropic/Kimi и другие templates. Поэтому размер
одного fallback не описывает стоимость всех моделей.

| Когда / источник | До → после | Причина и сила evidence |
|---|---|---|
| 2026-07-15, [#37181](https://github.com/anomalyco/opencode/pull/37181) | Model-specific выбор в runner → granular plugins | Boundary refactor; часть файлов перенесена без изменения текста. Есть targeted checks, нет отдельного behavioral benchmark |
| 2026-08-14, [#42638](https://github.com/anomalyco/opencode/pull/42638) | Fallback около 95 строк → 14 строк и conditional tool guidance | Diff подтверждает сокращение. В просмотренных body/comments/reviews подробной причины нет |
| 2026-08-16, [#42652](https://github.com/anomalyco/opencode/pull/42652) | Только enter/leave events → сверка reminder с durable mode при каждом request | Compact/revert удалял reminder, сохраняя mode; обратный revert оставлял stale reminder. Tests покрывают missing/stale/dedup/quoted cases |
| 2026-09-02, [#46753](https://github.com/anomalyco/opencode/pull/46753) | Пересобран GPT/Codex template; subagents требуют явного указания user либо применимых AGENTS/skill instructions | Автор связывает изменение со scope expansion Sol и адаптацией Codex; публичного efficacy A/B нет |
| 2026-09-02, [#46905](https://github.com/anomalyco/opencode/pull/46905) | Optional plan file → обсуждение в чате по умолчанию | Даже optional формулировка подталкивала к лишней записи файла. Одна строка и matching assertion не являются широким evaluation |
| 2026-09-09, [#48058](https://github.com/anomalyco/opencode/pull/48058) | Накопительный changelog/AGENTS/files → только нужное для продолжения; legacy summary переписывается под новый формат | Пять old/new historical replays; автор сообщает примерно вдвое меньшие summaries и reviewer pass без существенной потери |
| 2026-09-10, [#48276](https://github.com/anomalyco/opencode/pull/48276) | Compact template до mutable hooks → после hooks | Общий rewriter мог испортить служебную задачу. Regression фиксирует границу преобразования и итогового запроса |

[Текущий plan plugin](https://github.com/anomalyco/opencode/blob/d432a06b9c07bab0be4e8d642173b1cb14137f11/packages/core/src/plugin/plan.ts)
восстанавливает представление mode возле хвоста запроса, не переписывая старый
prefix. Это принцип синхронизации с native state. Сам по себе такой reminder
не даёт универсального запрета shell/remote side effects.

[Current compaction](https://github.com/anomalyco/opencode/blob/d432a06b9c07bab0be4e8d642173b1cb14137f11/packages/core/src/session/compaction.ts)
различает create/update, retained recent messages и native-compaction path.
Лимит 15 relevant files из #48058 — конкретный выбор продукта. Оснований
считать его оптимальным для ANSA или произвольного контекста в источнике нет.

## 5. Kimi: от усиления инициативы к согласованным, более коротким слоям

У legacy CLI [agent loader](https://github.com/MoonshotAI/kimi-cli/blob/9ab1286b8fe4e6bcd116949a27ce5e0ac3389c82/src/kimi_cli/soul/agent.py)
собирает Jinja template, environment, AGENTS и skills; YAML определяет tools.
У current Code
[profile-shared.ts](https://github.com/MoonshotAI/kimi-code/blob/f082f04a093a3e71a2263c5461d52507dc31f83a/packages/agent-core-v2/src/app/agentProfileCatalog/profile-shared.ts)
собирает несколько role/tool/project-dependent sections.
[Full compaction service](https://github.com/MoonshotAI/kimi-code/blob/f082f04a093a3e71a2263c5461d52507dc31f83a/packages/agent-core-v2/src/agent/fullCompaction/fullCompactionService.ts)
и [instruction](https://github.com/MoonshotAI/kimi-code/blob/f082f04a093a3e71a2263c5461d52507dc31f83a/packages/agent-core-v2/src/agent/fullCompaction/compaction-instruction.md)
образуют отдельный handoff, а не повтор system.md.

| Когда / источник | Изменение | Почему и граница вывода |
|---|---|---|
| Legacy, 2025-12-08, [#447](https://github.com/MoonshotAI/kimi-cli/pull/447) | Stringified context → content parts без thinking; summary task после history | Код и tests подтверждают форму. PR остался template; конкретный incident/model не указан |
| Legacy, 2026-03-02, [#1300](https://github.com/MoonshotAI/kimi-cli/pull/1300) | Custom /compact focus, ratio trigger и reserve | Product feature и boundary checks. Значение ratio не получено как универсальный optimum |
| Legacy, 2026-03-10, [#1392](https://github.com/MoonshotAI/kimi-cli/pull/1392) | Plan tools, UI/review и periodic reminders | Совместная реализация prompt+runtime workflow; вклад одного текста не изолирован |
| Legacy, 2026-03-25, [#1575](https://github.com/MoonshotAI/kimi-cli/pull/1575) | Усиление action bias и продолжения работы до результата | PR body пуст; соседний transport fix не доказывает причинную связь |
| Legacy, 2026-05-11, [#2229](https://github.com/MoonshotAI/kimi-cli/pull/2229) | Общий plan reminder → отдельные root workflow и subagent invariant | Старый текст требовал tools, которых у subagent нет; role-aware regression |
| Code, 2026-08-18, [#3028](https://github.com/MoonshotAI/kimi-code/pull/3028) | Удалена чрезмерная proactive установка | Пользователи хотели обсуждения, агент начинал реализацию; broad comparative experiment не опубликован |
| Code, 2026-09-01, [#3390](https://github.com/MoonshotAI/kimi-code/pull/3390) | System около 17 KB → 6.6 KB; static-first, tool minutiae ближе к tools | Заявлен внутренний coding A/B. Нет public sample sizes/model rows/raw results; более слабой модели, по автору, нужны некоторые concrete clauses |
| Code, 2026-09-03, [#3423](https://github.com/MoonshotAI/kimi-code/pull/3423) | Journal recovery pointer и бюджетные reminders | Сводки недостаточно, длинные journal lines обрезались при чтении; исправлены и текст, и read path |
| Code, 2026-09-07, [#3537](https://github.com/MoonshotAI/kimi-code/pull/3537) | Self-note → handoff + status прежних requests + continuation anchor после summary | После compact возобновлялась старая выполненная задача; последняя user-role summary воспринималась как новый запрос. Prompt-layer mitigation не закрывает authorization/TODO/background races |
| Code, 2026-10-23, [#4247](https://github.com/MoonshotAI/kimi-code/pull/4247) | Plan включает только пользователь; обязательный review, корректный exit, обычные вопросы допускают ответ текстом | Подробный behavior contract и заявленные 238 сравнительных запусков; менялись guards/UI/text вместе |
| Code, 2026-10-27, [#4451](https://github.com/MoonshotAI/kimi-code/pull/4451) | Согласованы verification scope, stopping rule и compact prefix; stale checks явно помечаются | Три серии 16+9+6 запусков не воспроизвели дефект и на старых вариантах: no-regression evidence, не подтверждение efficacy |

### Показательная цепочка противоречия

#4451 восстанавливает историю #1214 → #3390: compact prefix требовал заново
проверять заявления о завершённой работе, а обновлённый system предлагал доверять
сводке и не повторять уже сделанное. Общая verification инструкция также не
различала ответ на вопрос и изменение кода, не давала stopping rule и не ограничивала
реакцию на unrelated failures.

Изменение согласовало три места: что проверять перед завершением; как писать
summary о результатах, которые устарели после последующей правки; что повторять
после восстановления. Это убедительная логическая правка противоречия. Но автор
прямо пишет, что маленькие испытания не доказали изменения поведения и не дают
оснований обещать около 7% экономии, наблюдавшейся лишь в одной небольшой серии.
[Источник](https://github.com/MoonshotAI/kimi-code/pull/4451).

### Plan имеет несколько независимых свойств

[planModeInjection.ts](https://github.com/MoonshotAI/kimi-code/blob/f082f04a093a3e71a2263c5461d52507dc31f83a/packages/agent-core-v2/src/features/plan/injection/planModeInjection.ts)
выбирает full/sparse/reentry/exit текст.
[planService.ts](https://github.com/MoonshotAI/kimi-code/blob/f082f04a093a3e71a2263c5461d52507dc31f83a/packages/agent-core-v2/src/features/plan/planService.ts)
ограничивает Write/Edit к plan file и проводит review. Однако Bash и delegated
work следуют permission mode; #4247 прямо уточняет это для print mode.
Поэтому user-controlled plan, обязательный review и всеобщий hard read-only
executor нельзя считать одним и тем же свойством.

#3537 также меняет re-rendered injection при replay старых records. Это
source-specific решение; ANSA не должна заимствовать его без собственного
versioning/compatibility контракта.


## 6. Codex: model catalog, collaboration mode и два compact механизма

В текущем дереве
[model_instructions.rs](https://github.com/openai/codex/blob/5cb38e79842a0e0a6dca5bb97874163e8ee50129/codex-rs/prompts/src/model_instructions.rs),
[model_messages.rs](https://github.com/openai/codex/blob/5cb38e79842a0e0a6dca5bb97874163e8ee50129/codex-rs/prompts/src/model_messages.rs)
и [model manager](https://github.com/openai/codex/blob/5cb38e79842a0e0a6dca5bb97874163e8ee50129/codex-rs/models-manager/src/model_info.rs)
выбирают инструкции из model metadata/config/bundled fallback. Missing и явно
empty override различаются. Один commit клиента поэтому не фиксирует весь
полученный пользователем model prompt.

[Plan template](https://github.com/openai/codex/blob/5cb38e79842a0e0a6dca5bb97874163e8ee50129/codex-rs/collaboration-mode-templates/templates/plan.md)
принадлежит collaboration mode и связан с UI/parser для proposed_plan.
[update_plan handler](https://github.com/openai/codex/blob/5cb38e79842a0e0a6dca5bb97874163e8ee50129/codex-rs/core/src/tools/handlers/plan.rs)
работает с checklist и отвергает использование в Plan Mode. Из этого нельзя
выводить универсальный read-only guard на все остальные tools.

| Когда / источник | Изменение | Документированная причина / ограничение |
|---|---|---|
| 2025-08-12, [#2211](https://github.com/openai/codex/pull/2211) | После overhaul возвращены конкретные rg/chunk shell рекомендации; уточнены permission labels | Автор сообщает model confusion. Evals/local tests отмечены, количественных результатов в изученном обсуждении нет |
| 2025-09-03, [#3121](https://github.com/openai/codex/pull/3121) | Удалена часть formatting guidance | Модель выводила формулировку буквально; bug report, без comparative rate |
| 2025-09-12, [#3446](https://github.com/openai/codex/pull/3446) | Memento, earlier-user-message bridge и resume path | Реализация context handoff; одновременно меняются runtime и текст |
| 2025-10-31, [#6027](https://github.com/openai/codex/pull/6027) | Подробный memento → более структурированный handoff; user messages удерживаются отдельно | PR body пуст. Diff объясняет «что», но конкретное «почему лучше» не опубликовано |
| 2026-01-12, [#8961](https://github.com/openai/codex/pull/8961) | Permission/network/sandbox guidance собирается из актуального состояния | Уменьшение дублирования и рассогласования capabilities с prose; не общий prompt-quality benchmark |
| 2026-01-27, [#9977](https://github.com/openai/codex/pull/9977) | Plan меньше требует заранее расписанных patch/test-command/acceptance деталей | Автор считает планы избыточными; публичного численного evaluation нет |
| 2026-01-30, [#9786](https://github.com/openai/codex/pull/9786) | Typed/streamed proposed_plan, UI/parser/persistence; update_plan не вызывается в Plan Mode | Изменение output contract, а не только стиля |
| 2026-02-06, [#10574](https://github.com/openai/codex/pull/10574) | Новые remote compact results обрабатываются; существующая replacement_history replay сохраняется verbatim | PR body описывает более широкое переписывание, от которого отказались в review; см. раздел 9 |
| 2026-03-03, [#13284](https://github.com/openai/codex/pull/13284) | Plan ориентирован на 3–5 разделов по поведению, меньше speculative schemas/path inventories | Конкретное стилевое исправление; не доказательство универсального оптимального размера |
| 2026-05-04 и 05-15, [#20773](https://github.com/openai/codex/pull/20773), [#22809](https://github.com/openai/codex/pull/22809) | Remote compact V2 через normal Responses stream и compaction_trigger | Смена protocol/capability contract, не новая редакция публичного local summary prompt |
| 2026-06-01, [#25151](https://github.com/openai/codex/pull/25151) | Templates перенесены в codex-prompts | Организация ownership/compile-time; перенос файла сам по себе не behavioral improvement |
| 2026-06-21, [#29301](https://github.com/openai/codex/pull/29301) | Неизменённый план повторно выдаётся после уточняющего вопроса | UI должен сохранить implement action. Это не универсальное правило повторять уже сказанное |
| 2026-09-16, [#46026](https://github.com/openai/codex/pull/46026) | Явное разрешение model messages, source и missing/empty semantics | Provenance/config contract; metadata содержит GitOrigin-RevId и projection marker, но не раскрывает внутренние experiments |

[Local compact prompt](https://github.com/openai/codex/blob/5cb38e79842a0e0a6dca5bb97874163e8ee50129/codex-rs/prompts/templates/compact/prompt.md)
и [summary prefix](https://github.com/openai/codex/blob/5cb38e79842a0e0a6dca5bb97874163e8ee50129/codex-rs/prompts/templates/compact/summary_prefix.md)
описывают handoff с целями, решениями, важным контекстом, оставшейся работой и
ссылками. Есть custom compact override.

Текущий
[remote V2 attempt](https://github.com/openai/codex/blob/5cb38e79842a0e0a6dca5bb97874163e8ee50129/codex-rs/core/src/compact_remote_v2_attempt.rs)
и [task dispatch](https://github.com/openai/codex/blob/5cb38e79842a0e0a6dca5bb97874163e8ee50129/codex-rs/core/src/tasks/compact.rs)
используют provider capability и transient trigger, ожидая compaction output.
Серверный prompt здесь не опубликован. Нельзя выдавать local template за него
или переносить этот путь в ANSA, предполагая не проверенную для её SIWC profile
поддержку.

## 7. Pi: короткий core, extensions и эволюция context representation

[CLI system builder](https://github.com/earendil-works/pi/blob/7ce13c086831838c27aeabf2e7b38d1d825d6ac9/packages/coding-agent/src/core/system-prompt.ts)
собирает named sections по активным tools.
[Resource loader](https://github.com/earendil-works/pi/blob/7ce13c086831838c27aeabf2e7b38d1d825d6ac9/packages/coding-agent/src/core/resource-loader.ts)
добавляет project context, overrides и skills.
[AgentSession](https://github.com/earendil-works/pi/blob/7ce13c086831838c27aeabf2e7b38d1d825d6ac9/packages/coding-agent/src/core/agent-session.ts)
реально применяет sections, extensions и tool loadout. P2 имеет
[собственный prompt worker](https://github.com/earendil-works/pi/blob/7ce13c086831838c27aeabf2e7b38d1d825d6ac9/apps/p2/plugins/prompt/worker.ts)
и [durable compaction](https://github.com/earendil-works/pi/blob/7ce13c086831838c27aeabf2e7b38d1d825d6ac9/packages/durable/src/harness/compaction.ts);
это отдельные пути. [P2 README](https://github.com/earendil-works/pi/blob/7ce13c086831838c27aeabf2e7b38d1d825d6ac9/apps/p2/README.md)
называет приложение unsupported prototype. Изменение CLI budget в #11036 ниже
не переносится автоматически на durable: у него в этом snapshot остаётся
собственный summary output cap.

| Когда / источник | Изменение | Причина / evidence |
|---|---|---|
| 2025-10-17, [ffc9be8](https://github.com/earendil-works/pi/commit/ffc9be88679426fff067eae3831e399c4b0b7651) | Первое найденное добавление нынешнего coding-agent package: короткая роль, четыре tools, практические rules и cwd | WIP introduction, не доказанный оптимум; не утверждение об отсутствии более ранних прототипов |
| 2025-12-03, [6c2360a](https://github.com/earendil-works/pi/commit/6c2360af28348b12897dd516486d32436c262895) | Compact как handoff checklist с progress/constraints/paths/next steps | Feature introduction; UI/auto trigger ещё отдельно |
| 2025-12-29, [ac71aac](https://github.com/earendil-works/pi/commit/ac71aac0904518f04007edaf356f51bd7ad4c7bd), [2add465](https://github.com/earendil-works/pi/commit/2add465fbe44b15259e45fca3120cdb0f9cd6e0e) | Requested Markdown sections, затем сериализация conversation как данных | Native messages побуждали модель продолжать задачу вместо summarization. Изменилось представление входа, не только императив |
| 2026-01-13, [#694](https://github.com/earendil-works/pi/pull/694) | Progress по каждому tool_result → explicit DONE markers | Один шаг требует нескольких tools; parser tests не доказывают истинность заявленного завершения |
| 2026-05-18, [#4709](https://github.com/earendil-works/pi/pull/4709) | Markdown file boundaries → XML-style wrappers | AGENTS/CLAUDE содержали собственные headings; это конкретная boundary проблема, не универсальное превосходство XML |
| 2026-07-14, [f4e9ca7](https://github.com/earendil-works/pi/commit/f4e9ca7466b5576090d1093c27fe38d73909f3d2), [#6621](https://github.com/earendil-works/pi/issues/6621) | Текущая дата удалена из system | После смены дня на resume инвалидировался cache и рос local-model prefill; user incident, не cross-provider latency benchmark |
| 2026-09-16–17, [#9548](https://github.com/earendil-works/pi/pull/9548), [1629239](https://github.com/earendil-works/pi/commit/16292398af7270c6248a06f7a9da42aa0f91d413) | Prompt/tool patches в transcript; затем forced whole-prompt override вынесен в request-only projection | Resume/branch provenance; полные replacements разрушали structured sections и последующие updates |
| 2026-09-22, [#9908](https://github.com/earendil-works/pi/pull/9908) | Split-turn wording про наблюдавшуюся раннюю работу вместо реконструкции будущих сообщений | Provider refusals; автор выражает ожидание, а не публикует доказанный refusal-rate gain |
| 2026-10-01, [6f1072c](https://github.com/earendil-works/pi/commit/6f1072cc081f06b86a673bd142f03720d17afe15) | Codemode reference вынесен из eager prompt в docs | Автор сообщает примерно 5300→3300 default request tokens; это cost observation, без quality A/B |
| 2026-10-11, [15e7bdc](https://github.com/earendil-works/pi/commit/15e7bdc14cae38786c4cde24870ffce888e062cd) | Thinking исключён из compact serialization | Описан overflow: около 996K live context превратилось в 1.38M summary input |
| 2026-10-16, [#11036](https://github.com/earendil-works/pi/pull/11036) | CLI summary output caps отвязаны от trigger reserve; sizing передан provider layer | Reasoning съедал общий output allowance и обрывал текст; это не совет вообще отменить ограничения ресурсов |
| 2026-10-21, [#11058](https://github.com/earendil-works/pi/issues/11058) | Custom compact focus передаётся также split-turn summarizer | Один этап pipeline терял пользовательское условие |
| 2026-11-02, [#11680](https://github.com/earendil-works/pi/pull/11680) | Branch summary применяет context_edit | Иначе сводка возвращала failed responses, уже скрытые из live context |
| 2026-11-04, [#7950](https://github.com/earendil-works/pi/pull/7950) | Progress parser принимает больше thinking/prose/marker forms | Реальный ответ модели не всегда соответствует узкому маркерному формату; author-reported 43 tests |

Два ограничения особенно существенны.

Во-первых, [plan example](https://github.com/earendil-works/pi/blob/7ce13c086831838c27aeabf2e7b38d1d825d6ac9/packages/coding-agent/examples/extensions/plan-mode/index.ts)
выключает built-in edit/write, сохраняет другие active tools и применяет
[regex bash allowlist](https://github.com/earendil-works/pi/blob/7ce13c086831838c27aeabf2e7b38d1d825d6ac9/packages/coding-agent/examples/extensions/plan-mode/utils.ts).
Это не замена compiled read_only gate ANSA. Progress markers — UI сигнал модели,
не durable evidence результата.

Во-вторых, после 1629239 transcript сохраняет section patches, но forced
before_agent_start строка применяется только при формировании конкретного
запроса. Eval harness пришлось отдельно фиксировать forced variant. Поэтому
утверждение «Pi всегда сохраняет все реально отправленные prompt bytes в
transcript» неверно.

[Compact](https://github.com/earendil-works/pi/blob/7ce13c086831838c27aeabf2e7b38d1d825d6ac9/packages/coding-agent/src/core/compaction/compaction.ts)
запрашивает Markdown sections и проверяет transport/stop/tool-call условия;
это не semantic validation их содержания. Branch summary — отдельная задача.
В истории также есть revert cache-friendly primitives 2026-08-19 без
объяснённой в изученной metadata причины и поздний «tighten system prompt»
без опубликованного A/B. Эти пробелы нельзя заполнять выдуманной rationale.

## 8. DeepSeek Harness, Harness² и Hermes

### 8.1. DeepSeek Harness: тексты, cache layout и decision notes

[System registry](https://github.com/deepseek-ai/deepseek-harness/blob/39163dd30f703a79079e2d2446545cff3c453738/packages/core/system-prompt/src/index.ts)
рендерит упорядоченные sections;
[persona plugin](https://github.com/deepseek-ai/deepseek-harness/blob/39163dd30f703a79079e2d2446545cff3c453738/packages/preset/persona/src/index.ts)
допускает prefix/suffix/complete override. Dynamic runtime context имеет отдельное
представление. Поэтому «исходный prompt DeepSeek» зависит от deployment bundle.

Реальная plan policy задаётся в
[base bundle](https://github.com/deepseek-ai/deepseek-harness/blob/39163dd30f703a79079e2d2446545cff3c453738/packages/bundle/base/cordis.patch.yml):
исследовать, составить decision-complete plan, ждать формального review, не
приравнивать разговорное согласие к approval. Однако
[plan-mode README](https://github.com/deepseek-ai/deepseek-harness/blob/39163dd30f703a79079e2d2446545cff3c453738/packages/plan/plan-mode/README.md)
прямо говорит, что tools остаются callable; enforced restrictions задаются
sandbox/approval policy отдельно.

[Summarizer](https://github.com/deepseek-ai/deepseek-harness/blob/39163dd30f703a79079e2d2446545cff3c453738/packages/compaction/compaction-basic/src/summarizer.ts)
добавляет compact directive после system/tools/history prefix. Он просит
структурированный checkpoint, точные literals и consolidation старой summary,
ограничивает результат и сохраняет auxiliary provenance. Это app-owned путь;
его cache поведение зависит от provider, route и точного входа.

| Когда / commit | До → после | Документированная причина / evidence |
|---|---|---|
| 2026-07-21, [a0268d2](https://github.com/deepseek-ai/deepseek-harness/commit/a0268d25099c50f44b988d9c093af25e0032ca1f) | Другой summarizer system + flattened transcript → оригинальный prefix + trailing directive | Уникальный prefix заставлял заново обрабатывать историю. Request-shape tests; provider-wide cache hit-rate не измерен |
| 2026-07-30, [ed49f1d](https://github.com/deepseek-ai/deepseek-harness/commit/ed49f1d985205d212aea2b67b160d638c2efda4b) | Internal checkpoint должен быть на английском, literals сохраняются | Decision note опасается закрепления разговорного регистра в durable summary; model-quality experiment не приведён |
| 2026-07-31, [a43a0b5](https://github.com/deepseek-ai/deepseek-harness/commit/a43a0b5cffe1d96abbae4c24b8e8d4bc59d8fb98) | Жёсткое opening requirement смягчено до concise English prose | Сам факт виден в diff; почему понадобилось смягчение, inspected message не объясняет |
| 2026-09-06, [4079233](https://github.com/deepseek-ai/deepseek-harness/commit/40792330c0d534ef382bbf1fb44c9289323bbb27) | Stable persona prefix отделён от cwd/environment suffix | Notes сравнивают альтернативы и прямо не обещают измеренного cache-hit улучшения |
| 2026-09-21, [7520fe9](https://github.com/deepseek-ai/deepseek-harness/commit/7520fe94e1444b8c0a37038b074fbb937a881620) | Убраны дубли creator persona/tool/skill guidance | Автор сообщает first-turn tokens deepseek-v4-flash 9572→8975→8815. Один setup, без task-quality A/B |
| 2026-10-11, [36779dc](https://github.com/deepseek-ai/deepseek-harness/commit/36779dcf0998674d6d5bfa74a2cace01a501b2db) | Process-specific paths/URLs → runtime context; старые session paragraphs сохраняются | На replace-only Anthropic route новый prefix нарушал signed-thinking replay; source-specific failure и tests |
| 2026-10-19, [d3f486e](https://github.com/deepseek-ai/deepseek-harness/commit/d3f486e98e9fc39df705889add624603767d4beb) | Dispatch до ready plan guidance → fail-closed request construction | Cold/restored plan tests проверяют отсутствие раннего запроса и наличие правильной policy после ready |
| 2026-11-12, [9ebb02a](https://github.com/deepseek-ai/deepseek-harness/commit/9ebb02afb793f45c6ed6d1ed205f16378188ac30) | Summarizer route от latest attempted request → last successful assistant route либо configured target | Последняя попытка могла быть rejected/overflow; routing fixtures, не улучшение текста summary |

Особенно полезны сохраняемые в Git decision notes с Problem, Decision,
Alternatives, Consequences и Testing:
[prefix reuse](https://github.com/deepseek-ai/deepseek-harness/blob/a0268d25099c50f44b988d9c093af25e0032ca1f/.agents/notes/implemented/bug-fix/2026-07-21-compaction-summary-prefix-cache-reuse.md),
[English checkpoint](https://github.com/deepseek-ai/deepseek-harness/blob/ed49f1d985205d212aea2b67b160d638c2efda4b/.agents/notes/implemented/bug-fix/2026-07-31-english-compaction-checkpoints.md),
[environment suffix](https://github.com/deepseek-ai/deepseek-harness/blob/40792330c0d534ef382bbf1fb44c9289323bbb27/.agents/notes/implemented/bug-fix/2026-09-06-environment-prompt-suffix.md),
[process-specific values](https://github.com/deepseek-ai/deepseek-harness/blob/36779dcf0998674d6d5bfa74a2cace01a501b2db/.agents/notes/implemented/bug-fix/2026-10-11-process-specific-prompt-values-in-runtime-context.md).
Ссылки закреплены на исторических commit: позднее notes могли переместиться в
archive. Они объясняют intent, но не автоматически доказывают causal benefit.

### 8.2. Harness²: optimizer prompts, а не ещё один CLI Plan Mode

[core/prompts.py](https://github.com/google-research/harness2/blob/6868817518876de8fd170c6a8354daca273b6191/harness2/core/prompts.py)
содержит фазы propose, aggregate, compose_round, select и experience. Optimizer
меняет допустимые поверхности executor harness: system guidance, guardrails,
memory, skills/scripts/subagents и другие разрешённые файлы. Обычные runtime
plan/compact механизмы остаются у executor.

[OpenCode baseline systemprompt.md](https://github.com/google-research/harness2/blob/6868817518876de8fd170c6a8354daca273b6191/third_party/jb_runner/harness_baseline_oc/systemprompt.md)
— пустой additive overlay; stock provider prompt при этом остаётся.
Это не эксперимент «агент вообще без system prompt».

В reachable main history просмотрен 21 commit. Для выбранных prompts.py и
baseline systemprompt.md path history содержит только
[initial public import e0fb650](https://github.com/google-research/harness2/commit/e0fb65029feecd7f786ddd5b648c6f54aae7d86d),
2026-10-08; blob prompts.py совпадает с текущим. По этой публичной ancestry
нельзя восстановить предшествующие внутренние редакции. Это ограничение именно
выбранных путей/main ancestry, не утверждение обо всех branches или run outputs.

Важное изменение [#33](https://github.com/google-research/harness2/pull/33),
[aa02dbc](https://github.com/google-research/harness2/commit/aa02dbcdc353bcfb8f57a17f5514e942eb076b32),
2026-11-02, исправляет вход следующей попытки: после off-surface rejection
раньше терялись также допустимые edits, хотя feedback сообщал только об
отброшенных недопустимых. Новый retry сохраняет valid edits. Это repair состояния,
которое видит optimizer, а не улучшение prose. Автор сообщает fail-before/pass-after
regression и bounded tests/demo.

Evolution конкретного optimized harness может жить в
[run artifacts proposer](https://github.com/google-research/harness2/blob/6868817518876de8fd170c6a8354daca273b6191/harness2/core/proposer.py):
mission/system/response files, workspace и metadata с model, effort, attempts,
time, cost, error; fingerprint включает mission/system/context files. Git
исходников optimizer не заменяет эти данные.

Experience prompt предлагает различать helped/hurt/no clear effect/confounded,
не приписывать causal credit единственному удачному rollout и допускать отсутствие
полезного урока. Для ANSA это хорошая дисциплина экспериментального журнала.
Само наличие такого текста не доказывает отсутствие evaluation leakage;
общие результаты optimizer также не являются ablation одной prompt строки.

### 8.3. Hermes: цена всего prompt/tool surface

[System builder](https://github.com/NousResearch/hermes-agent/blob/5e9c0cd31e783605ada6bd6f01da505bf9b6b10b/agent/system_prompt.py)
собирает stable/context/volatile части.
[Plan prompt](https://github.com/NousResearch/hermes-agent/blob/5e9c0cd31e783605ada6bd6f01da505bf9b6b10b/agent/plan_prompt.py)
попадает в обычный queued user turn через
[CLI dispatch](https://github.com/NousResearch/hermes-agent/blob/5e9c0cd31e783605ada6bd6f01da505bf9b6b10b/hermes_cli/cli_commands_mixin.py).
В проверенной ветке нет переключения toolset/permission policy. Это локальный
вывод о /plan dispatch, не аудит отсутствия всех защит продукта.
Файл plan_limits.py относится к подписочным usage windows, а не к Plan Mode.

| Когда / источник | Изменение | Evidence и предел |
|---|---|---|
| 2026-08-30, [#98272](https://github.com/NousResearch/hermes-agent/pull/98272) | /plan skill → builtin command, normal-turn prompt сохранён | Команда выпадала из ограниченных Telegram/Discord menus. Исправление доступности, не новый read-only executor |
| 2026-08-30, [#98426](https://github.com/NousResearch/hermes-agent/pull/98426) | Rebuild prompt на compaction boundary | Долгие сессии имели старый prompt при обновлённых tools; contract tests, live e2e в PR оставлен отдельно |
| 2026-10-12, [#137084](https://github.com/NousResearch/hermes-agent/pull/137084) | Vault tools deferred/consolidated, schemas сокращены, общий Async handoff duplicate удалён | 7 tasks × 3 reps × 3 models; 63/63 обе стороны, total tokens −11%, non-vault −26%, vault +16% |
| 2026-11-04/05, [#86019](https://github.com/NousResearch/hermes-agent/pull/86019) | Explicit UNVERIFIED claim+basis на first/iterative compact | Inline qualifier мог исчезать и превращать гипотезу в факт; один illustrative transcript и regression coverage, не broad fidelity benchmark |

[Eval README #137084](https://github.com/NousResearch/hermes-agent/blob/3bf97898ac0ecfc8190202cae390c84004b0240f/evals/prefix_rediet/README.md)
уточняет, что browser/vault scripted, а terminal/files/cron/session DB реальные.
Обе стороны достигли ceiling на этой батарее. Рост стоимости vault объясняется
discovery round trips: меньше eager tokens не гарантируют меньшую стоимость
каждой задачи.

[Uncertainty rules](https://github.com/NousResearch/hermes-agent/blob/5e9c0cd31e783605ada6bd6f01da505bf9b6b10b/agent/context_compressor_prompt_rules.py)
добавлены summarizer-у, а не cached base prompt. Author-reported +79 tokens —
цена конкретного текста, не метрика общей эффективности. Связанная автором
научная статья в этом bounded исследовании отдельно не оценивалась.


## 9. Что дают git/PR metadata и почему их надо сверять

### 9.1. PR body может противоречить итоговому решению

У Codex [#10574](https://github.com/openai/codex/pull/10574) body обещает
удалять stale developer instructions при reconstruction старой
replacement_history. Но
[review discussion](https://github.com/openai/codex/pull/10574#discussion_r2771569015)
фиксирует отказ переписывать старую историю ради этого edge case. В
[финальном commit](https://github.com/openai/codex/commit/143daadb318853b10c81d40b43e2b976fe14a4d1)
добавлен test reconstruct_history_uses_replacement_history_verbatim; refresh
нового remote результата и replay уже сохранённого результата различаются.

Вывод исследования: body полезен как история intent, но утверждение о
реализованном поведении проверяется final diff. Комментарий reviewer сам по
себе также не заменяет проверку кода.

### 9.2. Номер в commit message не гарантирует доступный GitHub PR

Commit DeepSeek Harness
[7520fe9](https://github.com/deepseek-ai/deepseek-harness/commit/7520fe94e1444b8c0a37038b074fbb937a881620)
содержит #4745. Однако commit→pulls API вернул пустую association, а запрос
публичного PR #4745 — 404. Сам commit и diff доступны. Поэтому источник
здесь — commit/notes; отсутствующий public PR не подменяется выдуманной
ссылкой. Происхождение номера из этих данных неизвестно.

Обратный случай у Pi [#7950](https://github.com/earendil-works/pi/pull/7950):
automatic association не нашла PR, но прямой lookup подтвердил его и
merge metadata. PR создан в августе, merged в ноябре. Ни пустая association,
ни дата создания не устанавливают дату shipped behavior.

### 9.3. Git даты, provenance trailers и подпись имеют ограниченный смысл

Author date, committer date, PR created/updated/merged timestamps описывают
разные события. Rebase, cherry-pick, import и projection могут разделять их.
Старые commit dates DeepSeek предшествуют созданию публичного repository:
это не доказательство, что код был публичен в эти даты.

Codex [#46026](https://github.com/openai/codex/pull/46026) содержит
GitOrigin-RevId и projection metadata. Это следы процесса доставки, а не доступ
к внутренним обсуждениям или evaluation. Verified signature подтверждает
проверку подписи GitHub, но не качество prompt и не воспроизводимость
заявленных measurements. PR body/comments изменяемы; ledger сохраняет
доступные timestamps и дату наблюдения, не обещая immutable копию всей GitHub
discussion.

### 9.4. Import и rename ограничивают реконструкцию истории

- У Harness² выбранные prompt files появились одним public import; внутренние
  итерации до него не видны.
- P2-specific app resources импортированы из private repo
  [05dc8d2](https://github.com/earendil-works/pi/commit/05dc8d239d0857cc092687506cfbe88bde29a9a1).
  Предшествующая публичная история shared durable modules существует отдельно.
- OpenCode default → base → replacement system и Kimi koder → default
  требуют перехода на старые paths. Пустая история нынешнего отсутствующего
  файла не означает, что prompt не менялся.
- DeepSeek compact/compact-basic → compaction/compaction-basic
  прослежен через rename. Только нынешний path скрывал большую часть ранней
  эволюции.
- У Codex прослежены перемещения templates из core в отдельные crates.
  Это исследование Rust-lineage, не заявление о полном восстановлении самых
  ранних TypeScript prompts проекта.

### 9.5. Измерения нельзя складывать в рейтинг harnesses

Все значения ниже **сообщены upstream авторами**, не воспроизведены этим PR.

| Источник | Что и на чём измерено | Результат | Корректный вывод |
|---|---|---|---|
| [OpenCode #48058](https://github.com/anomalyco/opencode/pull/48058) | Пять historical compact replays, old/new exact history, gpt-6-astra#high | Среднее около 20.7K→10.3K chars, 5000→2600 output tokens; reviewer не нашёл material loss | Удачное малое сравнение стоимости/содержания summary; нет downstream task-success benchmark |
| [Kimi #3390](https://github.com/MoonshotAI/kimi-code/pull/3390) | Внутренний coding A/B, details не опубликованы | Около 17KB→6.6KB текста; автор сообщает сохранение/рост scores latest model | Documented intent и internal claim; публично воспроизвести сравнение нельзя |
| [Kimi #4451](https://github.com/MoonshotAI/kimi-code/pull/4451) | 16+9+6 bounded sessions на hosted Kimi high reasoning | Старые варианты тоже не показали целевую проблему | No regression в этих сценариях; efficacy и экономия не доказаны |
| [Pi 6f1072c](https://github.com/earendil-works/pi/commit/6f1072cc081f06b86a673bd142f03720d17afe15) | Default-tools request после переноса codemode reference | Примерно 5300→3300 tokens | Сокращение конкретного request; task quality отдельно |
| [DeepSeek 7520fe9](https://github.com/deepseek-ai/deepseek-harness/commit/7520fe94e1444b8c0a37038b074fbb937a881620) | Первый API request deepseek-v4-flash в конкретном setup | 9572→8975 intermediate→8815 final prompt tokens | Наблюдение fixed prompt cost; не general success improvement |
| [Hermes #137084](https://github.com/NousResearch/hermes-agent/pull/137084) | 7 задач × 3 повтора × 3 модели, 63 scored runs на arm | 63/63 обе стороны; total tokens −11%, non-vault −26%, vault +16% | Bounded cost/no-regression evidence с ceiling effect и discovery trade-off |

Размер текста, output tokens одной summary, total tokens всех turns и task
success — разные величины. На их основании нельзя объявить один prompt
«научно лучшим», усреднить проценты между проектами или обещать ANSA тот же
выигрыш. Также нельзя считать один marker после compact доказательством fidelity
длинной работы.

## 10. Что следует делать в ANSA

### 10.1. Сначала признать уже существующие механизмы

На baseline ANSA:

- [agent_epoch](https://github.com/junknewera/ansa/blob/a6f203173969c72f9b9e9b0a8ad873958215db6d/rust/crates/ansa/src/config/agents.rs)
  сохраняет base instructions, effective settings, initial AGENTS и policy
  prompt_summary_keep_two_v1. Base уже короткий; наблюдаемого основания
  заменять его большим чужим текстом нет.
- [Request::fingerprint](https://github.com/junknewera/ansa/blob/a6f203173969c72f9b9e9b0a8ad873958215db6d/rust/crates/ansa/src/provider/request.rs)
  включает provider/account/requested/returned model, exact instructions,
  epoch/state, tool revision/descriptors и request JSON.
- [request_for](https://github.com/junknewera/ansa/blob/a6f203173969c72f9b9e9b0a8ad873958215db6d/rust/crates/ansa/src/session/state.rs)
  уже выводит plan request из принятого mode, добавляет reminder и plan-specific
  provenance. [ToolRegistry](https://github.com/junknewera/ansa/blob/a6f203173969c72f9b9e9b0a8ad873958215db6d/rust/crates/ansa/src/tools/registry.rs)
  формирует compiled read_only advertisement; это сильнее ограничения prose.
- [Legacy compact](https://github.com/junknewera/ansa/blob/a6f203173969c72f9b9e9b0a8ad873958215db6d/rust/crates/ansa/src/context/legacy_compact.rs)
  сохраняет актуальный tail, использует original binding и пустой toolset,
  проверяет admitted response/отсутствие tool calls/refusal, непустой bounded text.
  Текст просит goal, constraints, decisions, files/results, unresolved и next actions
  с инструкцией не превышать 600 слов; runtime проверяет лимит 16 KiB, а не
  word count. Ни одна из этих констант не доказана как оптимальная.
- Unknown operation выводится из native intent/results; summary не может
  разрешить replay, снять pause или превратить unknown effect в Done.
  [AGENTS](../AGENTS.md) закрепляет exact epochs, append-only journal и
  запрет неявного provider/model/account switch.

Следовательно, предложенный manifest — удобное производное представление
существующего provenance для inspection/eval, а не новый canonical store.
Сначала достаточно показать, откуда взялся каждый prompt fragment, какая
policy выбрана и с каким существующим fingerprint связан запрос.

### 10.2. Два compatibility blocker перед любым улучшением wording

**Compact.** В legacy_compact.rs функция window сравнивает сохранённый
summary_response.request с заново построенным summary_request(original).
Тот использует hardcoded prompt. При чтении omh.summary.v1 admit снова проходит
эту проверку. Если просто заменить literal, старый сохранённый request больше
не совпадёт: возможен summary_response_request отказ. Это вывод из текущего
кода, не выполненная runtime reproduction.

**Plan.** В state.rs accepted event хранит next request; apply сравнивает его
с новым request_for(text, mode). Эта функция добавляет hardcoded plan suffix.
Простая правка suffix способна привести к session_request_conflict на старом
plan turn. Сохранить старый journal файл недостаточно: надо сохранить именно
старую семантику его восстановления.

Нужен явный version dispatch старой и новой редакций, а не угадывание policy по
тексту. Уже сохранённый exact_state позволяет рассматривать version metadata
без немедленной смены journal envelope, но это **не готовая реализация**:
текущий compact builder не выбирает редакцию по полю compaction; после compact
и plan state обёрнут в base/base_state. Resolver, unknown-policy handling и
совместимость C++ требуют отдельного проектирования.

[Сохранённый C++ compact](https://github.com/junknewera/ansa/blob/a6f203173969c72f9b9e9b0a8ad873958215db6d/src/compaction.cpp)
имеет аналогичный контракт. Старый fallback автоматически новую policy не
понимает. Честный unsupported отказ предотвращает повреждение, но не равен
успешному bidirectional compatibility gate. Откат журнала к старой копии после
эффектов не является решением.

### 10.3. Предлагаемая очередь отдельных изменений

Это рекомендации из исследования, не уже выполненная acceptance.

| Приоритет | Предложение | Для чего / условие |
|---|---|---|
| P0, prerequisite | Специфицировать versioned reconstruction plan и compact | Старые запросы восстанавливаются byte-exact; новая policy явна; unknown/contradictory metadata не принимается эвристически |
| P1 | Derived prompt inspection/eval manifest поверх существующего fingerprint | Source/template revisions, mode, tools и compact policy видны reviewer; secret data и второй journal не появляются |
| P1 | Экспериментальная handoff редакция: active ask, request status, evidence/verification scope, stale/uncertain/blocked | Нацелена на классы потерь, описанные в Kimi/Hermes/Pi; эффект требует fidelity и downstream continuation evaluation |
| P1 | Regression matrix для mode/compact/restart/branch-подобных projection сценариев, где ANSA их поддерживает | Сохраняет существующий native authority; не добавляет чужие неподдержанные функции ради теста |
| P2 | Сокращение только обнаруженных дублей base/tool/project instructions | Замерять полный effective request и discovery cost; не минимизировать количество строк как цель |
| P2 | Token-aware budget и выбор summary output allowance | Продолжение уже открытой TODO; учитывать реальный model/provider contract, reasoning и retained tail, а не копировать чужую константу |

Текущие UX/benchmark задачи и отдельный M10 этим proposal не переставляются.
Документы и frozen offline eval design можно подготовить сейчас. Runtime
изменения остаются отдельным scope с применимыми gates; существующее разрешение
на прошлый M9 pilot не означает разрешение новых live calls. Пока C++ fallback
входит в compatibility контракт, его reopen/append проверка обязательна.

### 10.4. Кандидат содержания compact, который стоит проверять

Это оригинальная схема для эксперимента ANSA, не копия чужого system prompt
и не новый production format:

1. **Active request:** текущая цель пользователя, ограничения и ближайшее действие.
   Summary не переписывает сохранённый pending user и не объявляет новую
   авторизацию.
2. **Request state:** прежние задачи разделены на completed, active, blocked,
   superseded; законченный ранний запрос не становится снова открытым.
3. **Decisions and evidence:** принятые решения, точные file/artifact/operation
   references, результат проверки и область, которую она покрывает.
4. **Verification state:** checked и относящееся к текущей версии; stale после
   последующей правки; unverified assertion; hypothesis с основанием.
   Слова «проверено» недостаточно без ссылки на наблюдаемое evidence.
5. **Pending and unknown:** незавершённые операции/вопросы/отказы с точными
   ссылками. Native unknown-effect state остаётся authority.
6. **Continuation:** что делать дальше, что уже можно восстановить из journal
   или файлов и не нужно копировать в summary.

У каждого поля должна быть проверяемая причина сохранения. Например,
passed test без связи с версией кода может провоцировать ложное завершение,
а безусловное «перепроверь всё» — повторные действия и scope expansion.
Факты, которые можно надёжно восстановить из artifact, допускают ссылку вместо
полного копирования; путь должен действительно существовать и быть доступен.

Для plan полезны goal/constraints, chosen approach, significant changes,
verification и unresolved decisions. Сначала нужно установить конкретный
дефект существующего короткого ANSA reminder. Ни «всегда пять разделов», ни
obligatory plan file, ни model-generated DONE markers не нужны без такого
дефекта. Compiled ограничения и admission продолжают работать независимо от
формата плана.

### 10.5. Минимальный план проверки будущего prompt PR

| Семейство сценариев | Что должно проверяться |
|---|---|
| Legacy replay | Старые basic/plan turns и omh.summary.v1 читаются с прежними exact requests; C++↔Rust reopen/append в поддерживаемом формате |
| Version dispatch | Новая policy выбирается из явного сохранённого state; missing/unknown/contradictory policy не вызывает silent rewrite |
| Active task | После нескольких выполненных задач и нового запроса compact продолжает новый запрос; tail/user bytes не подменены summary |
| Repeated compact | Три и более compactions не теряют обязательные constraints и не превращаются в неограниченный changelog |
| Verification freshness | Проверка прошла до изменения покрываемого кода → результат stale; неизменённый проверенный результат не требует повторения без причины |
| Epistemic status | Hypothesis/rejected alternative/unknown остаются отличимы от наблюдаемого факта |
| Quoted instructions | Команды внутри процитированной истории не исполняются summarizer-ом и не становятся authority после восстановления |
| Plan/basic transitions | Compact/restart/Resume сохраняют принятую mode и tool snapshot; stale/quoted reminder не меняет mode |
| Effects and cancellation | Summary не снимает effect_unknown, не разрешает retry и не заявляет rollback совершённого эффекта |
| Cost and budget | Измерены input/output/cached/total usage там, где provider их сообщает, latency, число turns/tools и качество продолжения |
| Recovery pointers | Ссылки реально позволяют восстановить нужное evidence без повторного side effect; отсутствие данных явно отражено |

Для model comparison следует зафиксировать dataset и held-out cases, prompt
revisions, model/provider/account binding, effort, toolset, budget и evaluator.
Сравнивать baseline и candidate попарно на одинаковых исходных состояниях,
заранее выбрать число повторов, менять порядок runs; использовать seeds только
там, где они действительно поддержаны. Не маскировать adapter/routing change
под wording ablation.

Отдельно измерять summary fidelity и успешность следующего шага. Проверка
headings или отсутствие tool calls недостаточны. Success и критические
нарушения должны проверяться независимыми наблюдаемыми условиями, а не только
моделью, которая написала summary. Считать matched tasks и распределение
cost/latency, сохранять failed attempts; маленькое no-regression сравнение так
и называть. Публичные данные здесь не задают достаточный statistical sample
size для ANSA — его выбирают под конкретную проверяемую гипотезу.

## 11. Как хранить причины собственных prompt изменений

Для будущего ANSA prompt PR полезен короткий record рядом с тестами/eval:

- failure case и точный source/baseline revision;
- изменённый layer: текст, loader, tools, state, parser либо transport;
- semantic before/after и rationale, включая отвергнутые альтернативы;
- affected models/modes, compatibility/replay policy и выход из изменения;
- dataset/evaluator/budget, exact effective-request fingerprint и run artifacts;
- отдельно structural tests, observed behavior, стоимость и неизвестное;
- итог review с обновлением описания при расхождении с первоначальным PR body.

Такой record не требует вводить отдельную подсистему памяти. Он делает
проверяемыми и положительный результат, и результат «не воспроизвелось».
Особенно полезно сохранить версию evaluator: изменение oracle может
объяснять apparent gain сильнее, чем новая формулировка.

## 12. Coverage, воспроизводимость и ограничения

Основные просмотренные history slices:

| Репозиторий | Coverage |
|---|---|
| OpenCode | Current compaction 100+4 entries, обе страницы; current system 3, plan 13; переходы старых prompt paths |
| Kimi CLI | System 19 плюс older koder path 15; compact text 2, runtime 18 |
| Kimi Code | V2 system 18, compaction instruction 5, full plan reminder 2 |
| Codex | Шесть pinned path histories, страницы не насыщены: old base 16, old/new compact 3/1, old/new plan 25/2, resolver 15; moves между crates |
| Pi | Selected current/older CLI, compaction, plan, durable/P2 paths; per-path counts, retrieved pages и pagination limits — в ledger |
| Harness² | Все 21 reachable main commits в срезе; выбранные prompt/baseline paths по одному commit |
| DeepSeek Harness | Current system registry 81, persona 9, plan 65, summarizer 8 плюс old summarizer path 30; выбранные subtrees |
| Hermes | Bounded builders/dispatch и 4 события; первая system_prompt.py страница 50 entries насыщена, полной истории не заявляется |

Для крупных DeepSeek/Hermes trees recursive requests не завершились успешно;
использованы directory/file reads и pinned path queries. Это не полный
repository-wide search. История может быть squash/import/rewrite; API
association и общедоступность discussions неполны. Remote model catalogs,
server compact prompts, private evals и run artifacts, не опубликованные
авторами, не восстанавливаются предположениями.

Исследование выполнено через GitHub file/commit/PR API, не через локальный
clone всех репозиториев. Для независимого повторения на clone подходят:

    git log --follow --format=fuller -- path/to/prompt
    git show COMMIT -- path/to/prompt path/to/loader path/to/tests
    git log -G 'relevant phrase' -- path/to/directory
    git blame COMMIT -- path/to/prompt
    gh api repos/OWNER/REPO/commits/COMMIT/pulls
    gh api repos/OWNER/REPO/pulls/NUMBER
    gh api repos/OWNER/REPO/pulls/NUMBER/comments

Это рекомендуемые команды reviewer, не журнал исполненных здесь команд.
Blame показывает последнего изменившего строку, не достаточную rationale;
--follow не восстанавливает закрытые pre-import итерации. Для финального
вывода нужны также loader, merged diff и review.

Полные метаданные выбранных событий и источников находятся в
[evidence ledger](research/prompt-evolution-evidence.json). Идеи в разделе 10 —
кандидаты отдельных изменений. Этот research PR не доказывает production
fidelity нового prompt, не закрывает long-context acceptance и не меняет
проверенный runtime checkpoint.
