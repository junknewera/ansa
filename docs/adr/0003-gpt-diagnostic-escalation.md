# ADR 0003: bounded GPT diagnostic escalation

- **Статус:** Proposed, design-only.
- **Дата:** 2026-11-13.
- **Baseline:** [main@8f37af0](https://github.com/junknewera/ansa/tree/8f37af075fed5918afc1fafd9d7167271e8c8d26), Rust M4 GO; production/default C++20.
- **Связанный proposal:** [PR #3](https://github.com/junknewera/ansa/pull/3), [ADR 0002 на cac4f12](https://github.com/junknewera/ansa/blob/cac4f12c8063f54c032bbd532f59e047a59891f2/docs/adr/0002-self-hosted-orchestration.md). ADR 0002 ещё не находится в `main`; номер 0003 сохраняет порядок независимых proposals.
- **Спецификация:** [Loop engineering](../LOOP_ENGINEERING.md); [prompts](../LOOP_ENGINEERING_PROMPTS.md).

## Context

Повторяющийся repair без новых проверяемых фактов может зациклиться на неверной гипотезе. Пользователь предложил после нескольких loops передавать проблему более сильной модели для диагностики, затем продолжать исполнение. Нужна GPT-версия, совместимая с собственным native ANSA runtime и direct SIWC.

Исследования feedback-based repair поддерживают разделение диагноста и исполнителя, но не доказывают пользу конкретной пары, порога три или subscription economics. Fresh context и дополнительный compute могут объяснять улучшение без сильного advisor. Поэтому включение функции требует собственного comparative eval.

## Decision

### 1. Детерминированная эскалация

Предлагаемый pilot profile: `gpt-6.1-sol`/`medium` worker → три завершённые attempts без verified progress → fresh `gpt-6-astra`/`high` advisor → один discriminating experiment → максимум две guided attempts → optional Astra takeover максимум две attempts либо Blocked.

Общий cap — семь write attempts на task; phases не добавляют slots сверх него. Если local phase потратила больше трёх slots до stagnation, поздним phases остаётся меньше. Один diagnostic incident на task. Порог и caps конфигурируемы и проверяются eval; profile выключен по умолчанию до feature acceptance.

Это opt-in расширение начального ограничения ADR 0002 «initial + два repairs», а не незаметное увеличение общего default budget. Для legacy orchestration profile действует прежняя policy. Нельзя объединить оба бюджета или прибавить семь попыток поверх старого лимита.

### 2. Попытки и прогресс принадлежат host

Attempt slot резервируется до writer dispatch и не возвращается после неудачи. Tool/HTTP rounds и повторная доставка receipts не создают новый attempt. No-progress streak меняется только после итогового verification outcome; intermediate mechanical PASS ждёт required reviewer/integration/final gates.

Прогресс — строгое улучшение относительно high-water checkpoint по frozen acceptance evaluator, без потери защищённых критериев. Текст модели, новый patch, полезное исследование и смена контекста не обнуляют streak. Infra/inconclusive результаты не увеличивают и не сбрасывают streak, но расходуют бюджет и имеют отдельный retry cap. Unknown effects блокируют новый writer.

### 3. Native ANSA и direct SIWC

Controller — Rust code будущего Orchestrate. Worker/advisor/reviewer — отдельные native sessions того же AgentRuntime. В MVP один active model child и один writer. Advisor получает compiled read-only ToolSnapshot; takeover создаёт новый basic child с отдельным writer grant. Сама модель не повышает свои права.

Используется собственный direct SIWC adapter и действующий account binding. Model/effort availability проверяются для этого route; public model card не подтверждает entitlement. Не вводятся Codex SDK/app-server, внешний agent CLI, платный fallback, неявная подмена аккаунта или модели. Новые role sessions не получают дополнительный subscription allowance.

### 4. Evidence перед интерпретацией

Diagnosis получает цель, frozen manifest, snapshot полного relevant content, environment, actual commands/results/logs, историю attempts и оставшийся budget. Summary worker — интерпретация; raw evidence доступно advisor через scoped reads.

Advisor возвращает причину, альтернативы, опровержимое предсказание, минимальный эксперимент и conditional plan. Host валидирует identity/scope, замораживает типизированные evaluation predicates и выполняет разрешённый эксперимент. Refuted/inconclusive advice не применяется как подтверждённое. Snapshot/policy/manifest mismatch делает advice stale. Для переноса guidance на следующий candidate нужна явная новая binding; encrypted reasoning другой сессии не копируется в текстовый handoff.

### 5. Durable control и stop

Task decisions/ledger находятся в отдельном versioned orchestration journal. Legacy session v1 и семантика RequestId не меняются. Child journal остаётся authority для собственной истории; root импортирует проверенные receipts идемпотентно.

Запись результата attempt, его classification и reducer update выполняются как один durable transition после требуемых observations. Imported receipt и finalized evaluation имеют разные dedup guards; replay завершает pending decision/intent даже без нового receipt. Next action имеет устойчивую identity и вычисляется из phase_before. Lost ACK reconciles по прежнему child store/CommandId/payload; missing/corrupt store блокирует продолжение. Между root/child нет общей транзакции. Ровно один учтённый transition не означает exactly-once внешнего side effect или model billing.

Root caps охватывают все calls, retries, Resume, compact, advisor, experiment и final review. Они переживают restart и новую session. На каждом dispatch проверяются stop, deadline, policy, budget и native recovery. Late known outcomes сохраняются, но не запускают новый loop. Byte limits и cancel не обещают точного in-flight token/credit cap на SIWC.

### 6. Acceptance отдельно от diagnosis

Успешный diagnostic report, targeted-test PASS и worker Done не равны Accepted. Требуются сохранённые host verification receipts, отдельный fresh final review, serial integration и gates на exact integrated version. Любое изменение review target tuple после integration требует fresh review соответствующей версии. Missing/zero/skipped required tests не считаются успехом. Если final-gate reserve отсутствует, выдаётся partial/blocked result.

## Alternatives

| Вариант | Решение |
|---|---|
| Продолжать Sol / повысить effort | Обязательные baselines; возможно лучший default для части задач |
| Свежий Sol worker или Sol advisor | Обязательные controls свежего контекста и отдельной роли |
| Astra с начала или takeover на тупике | Сравнить полную стоимость и rescue; advisor не предполагается выгоднее |
| Советник после каждого ответа/tool call | Отклонён для MVP: неверная единица loop и лишний расход |
| Бесконечные reviewer-of-reviewer | Отклонено: неограниченное продолжение без гарантии нового evidence |
| Rewrite skills после любой ошибки | Вне MVP; learning candidate требует отдельного versioned eval |

## Consequences

Получаем наблюдаемую, воспроизводимую policy выхода из тупика и возможность отказаться от неверного совета. Платим дополнительными inference, context transfer, verification и сложностью durable control. Ошибки GPT ролей могут коррелировать; более сильная модель не заменяет тесты и scope checks.

Production sequencing сохраняет [migration plan](../RUST_MIGRATION_PLAN.md): новая feature после verified M10 GO и соответствующих O1–O4. LE1–LE3 — offline/synthetic implementation gates; LE4 — отдельно разрешённый native pilot. Этот docs PR не запускает M5, live calls, cutover, merge или production enablement.

## Revisit criteria

Сравнить одинаковые stuck snapshots: continuation, higher effort, fresh worker, same-model advisor, Astra advisor и takeover. Затем сравнить complete pipelines со single Sol/Astra от исходной задачи, включая sunk failures. Зафиксировать tradeoff до eval; не заявлять equal-cost при неизвестных credit/usage данных.

Если advisor не даёт измеренной пользы против controls или увеличивает harmful regressions/false acceptance, оставить profile выключенным либо сузить routing к доказанному классу задач. Точный threshold, модель и budgets пересматриваются по данным; native-only authority/recovery invariants сохраняются.
