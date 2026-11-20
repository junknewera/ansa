# Workspace protocol v1

Implementation target: этапы 1–3 [design](TERMINAL_WORKSPACE_DESIGN.md).
Текущий проверенный runtime - [PROJECT_STATE](PROJECT_STATE.md).

Endpoint создаётся явно через `--workspace-dir PATH` в private directory.
Он привязан к исходной session/incarnation Carbon. Переход Carbon в другой
разговор не меняет attachment. Unix socket проверяет peer UID; это локальная
граница API для процессов пользователя. Runtime, journal и auth остаются одними.

Wire format - UTF-8 JSONL v1. Handshake содержит `version`, `role`, `client`.
Роль viewer разрешает snapshot/resync, страницы истории и показ результатов. Editor grant выдаёт
owner при запуске; клиент не может выдать его себе. Controller, GetContext,
Resume и SetGeneration через bridge запрещены. Команды несут session,
incarnation и ID; действия над turn требуют также turn и request. Успешная
передача команды означает posted, durable acceptance подтверждается отдельно.
Reconnect сам по себе не повторяет действие и не меняет ID.

Editor capability `submit_message` передаёт полный текст, turn, target и
expected_mode. При target=null создаётся Submit, при точном request - Steer.
Stable native ID включает hash клиентского namespace и исходный message ID.
`message_lookup` возвращает метаданные исходного Submit/Steer и hash текста,
без самого текста или tool output. `admission_durable` подтверждает сохранение
сообщения; `state_durable` отдельно подтверждает сохранение показанного состояния.
Отсутствие receipt не доказывает, что переданный ранее запрос не будет принят.
Клиент повторяет только bounded status reads. `cancel_message` использует
подтверждённые turn/request своей задачи и отдельную priority очередь runtime.

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
| Дескрипторы артефактов / кэш документов | 384 / 64 |
| Чтение артефактов | Очередь из 8, один filesystem worker |
| Поиск старой версии в Neovim | До 16 страниц, один повтор после вытеснения grant |

Analysis MVP предоставляет `dataset_inspect` и `python_execute`: второй запускает
фиксированный помесячный sum для CSV через pinned Python/Matplotlib. Он принимает
путь, названия date/value columns и report_key; произвольный Python source
в этой версии не передаётся. Дата - YYYY-MM-DD, числа - до 18 целых и 6 дробных
цифр. Строки с пустой датой или значением учитываются отдельно и пропускаются.
Точные итоги сохраняются в CSV/Markdown; график использует их числовое представление.

Worker ограничен 60 с wall time, 45 с CPU, 1 GiB address space, 64 FD и 16 MiB
на файл. Store резервирует 24 MiB на незавершённую операцию внутри общей quota
64 MiB; известный failed/cancelled result снимает резерв. Исходный dataset
сохраняется отдельным неизменяемым snapshot. Начальные пустые строки хранятся
отдельно, если они есть. Проверяются хеши исходных байтов и данных для worker.
Durable process stdout содержит
hash всех inputs, summary и каждого output. Recovery сверяет весь набор и
дописывает receipt/outcome без нового запуска. Несовпадение требует явного
восстановления. Descriptor доступен viewer только после canonical outcome;
наличие execution receipt само по себе не подтверждает committed artifact.

У проекции свой cursor, привязанный к runtime incarnation. Это не journal
offset. При пропуске обновлений клиент получает gap и полный заменяющий
snapshot с through cursor. Медленный socket не задерживает Carbon или Cancel.
Превышение лимита явно отклоняется. Закрытие viewer не отменяет turn.

Прямые editor writes, отдельный headless frontend, persistent kernels и MCP
относятся к этапу 4. Файлы и proposed diff открываются без автоматического save.
`artifact_catalog` читает correlated canonical TimelinePage. `ToolFinished`
сам по себе не выдаёт доступ: его outcome ещё может ожидать fsync. `artifact_read`
принимает только опубликованные ID/revision и сверяет receipt, hash, размер,
тип и права файла. PNG возвращается проверенным локальным путём, Markdown/CSV -
также текстом. Кэш документов пополняется canonical результатами
`editor_open_file` и `editor_propose_diff`; `document_read` читает этот кэш.
После вытеснения клиент заново получает grant из страниц журнала.

Neovim и плагины закреплены в [workspace-neovim.nix](../nix/workspace-neovim.nix).
Renderer сначала проверяет ответ терминала на RGB и PNG queries. APC replies
с собственными случайными ID отделены от ввода, включая bracketed paste;
прочие байты передаются прежнему обработчику Neovim. Закрытие attachment,
буфера и смена revision очищают только принадлежащие viewer placements.
Отложенные callbacks сверяют attachment, буфер, revision и changedtick.
SSH, tmux, несколько UI и `NO_COLOR` используют текстовый fallback.

Графическая приёмка использует настоящие Kitty 0.49.2 и Ghostty 1.3.1
в изолированном Weston с software GL. Снимки проверяют видимые pixels при
resize, scroll, переключении буфера, замене revision и очистке. Эта проверка
отделена от controlled-response PTY тестов capability negotiation.
