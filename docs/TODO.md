# Отложенные возможности ansa

Локальный многошаговый агент реализован с обычным текстовым CLI. Следующие возможности развиваются отдельными milestones.

- MCP: подключение серверов, их поиск и запуск для задачи, управление lifecycle, изменение tool snapshots в сессии и корректный replay. Примеры будущих задач - управление приложениями и играми через подходящий MCP.
- Внешние hooks. Политика удаления и перенос в TRASH должны подключаться пользовательским hook вне проекта; встроенного trash-правила сейчас не добавляем.
- Режим Ask как альтернатива full access.
- Интерактивный sudo через отдельный пользовательский канал; пароль не попадает в модель, journal или логи. В текущем этапе повышение привилегий запрещено.
- Orchestrate: координатор и дочерние агенты, распределение задач, bounded lifecycle и отмена, владение файлами, события состояний, сбор и проверка результатов. После реализации становится третьим режимом в переключателе Tab рядом с basic/plan. Визуальная основа остаётся Carbon; сведения об агентах раскрываются в области диалога.
- Loop engineering внутри будущего native Orchestrate: [GPT diagnostic escalation](LOOP_ENGINEERING.md), [ADR 0003](adr/0003-gpt-diagnostic-escalation.md), [prompts](LOOP_ENGINEERING_PROMPTS.md). LE0 design-only; LE1–LE4 после verified M10 и соответствующих O1–O4: offline reducer/budgets → native synthetic sessions → evidence/experiment/final gates → отдельно разрешённый paired live eval. Default enablement зависит от измеренной пользы, M5 и другие migration scopes не расширяются.
- Отдельный TUI поверх общего typed frontend/runtime API; Carbon CLI уже использует производный transcript canonical journal и preview/commit/discard со snapshot/resync.
- Token-aware context budget и проверка качества compact на длинных задачах.

Другие открытые технические ограничения и результаты предыдущих milestones - в [PROJECT_STATE.md](PROJECT_STATE.md).
