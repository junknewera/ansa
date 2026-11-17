# Workspace protocol v1

Implementation target: этапы 1–3 [design](TERMINAL_WORKSPACE_DESIGN.md).
Текущий проверенный runtime - [PROJECT_STATE](PROJECT_STATE.md).

Endpoint создаётся явно через `--workspace-dir PATH` в private directory.
Он привязан к исходной session/incarnation Carbon. Переход Carbon в другой
разговор не меняет attachment. Unix socket проверяет peer UID; это локальная
граница API для процессов пользователя. Runtime, journal и auth остаются одними.

Wire format - UTF-8 JSONL v1. Handshake содержит `version`, `role`, `client`.
Роль viewer разрешает snapshot/resync и страницы истории. Editor grant выдаёт
owner при запуске; клиент не может выдать его себе. Controller, GetContext,
Resume и SetGeneration через bridge запрещены. Команды несут session,
incarnation и ID; действия над turn требуют также turn и request. Успешная
передача команды означает posted, durable acceptance подтверждается отдельно.
Reconnect сам по себе не повторяет действие и не меняет ID.

DTO содержит подтверждённые реплики, явно обозначенный preview, состояние
turn и проверенные descriptors артефактов. Внутренние credentials, fork seed,
opaque reasoning и raw tool outputs не сериализуются. Пользовательский текст
передаётся как текст; API не обещает удалить секрет, который пользователь
сам включил в сообщение.

Пределы первой версии:

| Ресурс | Предел |
|---|---:|
| Одновременные клиенты | 8 |
| Входная строка до JSON parse | 128 KiB |
| Глубина JSON / число узлов | 16 / 4096 |
| Исходящий DTO | 512 KiB |
| Ответы одному клиенту | 4 DTO, до 2 MiB |
| Обновления проекции | Один заменяемый snapshot на клиентскую подписку |
| Handshake / socket write | 2 с |
| Страница истории | До 64 записей / 256 KiB исходного текста |
| Выделение или контекст редактора | 16 KiB |
| Editor request | 2 с |
| Analysis jobs | 1 на сессию, до 60 с |
| Dataset / blob / session artifacts | 4 / 16 / 64 MiB |
| PNG | До 8 миллионов pixels |

У проекции свой cursor, привязанный к runtime incarnation. Это не journal
offset. При пропуске обновлений клиент получает gap и полный заменяющий
snapshot с through cursor. Медленный socket не задерживает Carbon или Cancel.
Превышение лимита явно отклоняется. Закрытие viewer не отменяет turn.

Прямые editor writes, отдельный headless frontend, persistent kernels и MCP
относятся к этапу 4. Файлы и proposed diff открываются без автоматического save.
Графический этап требует отдельной реальной приёмки backend; Markdown и ссылка
на PNG сами по себе не закрывают проверку отображения изображений.
