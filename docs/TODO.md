# Отложенные возможности ansa

Локальный многошаговый агент реализован с обычным текстовым CLI. Следующие возможности развиваются отдельными milestones.

- MCP: подключение серверов, их поиск и запуск для задачи, управление lifecycle, изменение tool snapshots в сессии и корректный replay. Примеры будущих задач - управление приложениями и играми через подходящий MCP.
- Внешние hooks. Политика удаления и перенос в TRASH должны подключаться пользовательским hook вне проекта; встроенного trash-правила сейчас не добавляем.
- Режим Ask как альтернатива full access.
- Интерактивный sudo через отдельный пользовательский канал; пароль не попадает в модель, journal или логи. В текущем этапе повышение привилегий запрещено.
- Orchestrate: [proposed ADR 0002](adr/0002-self-hosted-orchestration.md), [исследование и O1–O4 roadmap](ORCHESTRATION_RESEARCH.md), [prompts](ORCHESTRATION_PROMPTS.md). Default implementation отдельным feature после verified M10 GO; M0–M10 scope неизменен. MVP: deterministic coordinator, один Codex builder, fresh native reviewer, обычный verification runner и canonical task journal. Internal subagents и parallel builders выключены; максимум два builders — отдельный этап после eval. Controlled worktrees, unknown-effect reconciliation, bounded repair и final-SHA acceptance обязательны. После implementation/gates режим становится третьим в Tab рядом с basic/plan; визуальная основа остаётся Carbon.
- Отдельный TUI поверх общего typed frontend/runtime API; Carbon CLI уже использует производный transcript canonical journal и preview/commit/discard со snapshot/resync.
- Token-aware context budget и проверка качества compact на длинных задачах.

Другие открытые технические ограничения и результаты предыдущих milestones - в [PROJECT_STATE.md](PROJECT_STATE.md).
