# ansa

Локальный агент для работы с проектом. Читает и ищет файлы, запускает команды, сохраняет разговор между запусками. Работает в любом каталоге, в том числе без Git.

Новая инженерная сессия начинается с [AGENTS.md](AGENTS.md) и [текущего состояния проекта](docs/PROJECT_STATE.md).

## Обычный CLI

Установите Nix и соберите программу из каталога репозитория:

```sh
nix build --no-update-lock-file
./result/bin/ansa --login
./result/bin/ansa
./result/bin/ansa 'Изучи проект и исправь ошибку'
./result/bin/ansa --continue
./result/bin/ansa --session SESSION_ID
./result/bin/ansa --cwd ~/Downloads 'Найди большие файлы и объясни результат'
```

Без аргумента задачи открывается Carbon CLI: диалог над строкой ввода и компактный статус инструментов. `NO_COLOR` и `TERM=dumb` отключают цвета.

В Carbon два нажатия Tab из `Work` включают `Orchestration`. При входе через
ChatGPT ansa запрашивает строгий формат Responses API и проверяет, что провайдер
подтвердил схему. При ошибке черновик остаётся в поле ввода. Помощники на этом
подключении выполняются по одному.

Запуск с OpenRouter:

```sh
printf '%s\n' "$OPENROUTER_API_KEY" | ./result/bin/ansa --openrouter-login
./result/bin/ansa --openrouter
./result/bin/ansa --openrouter --continue
```

В этом режиме выбран DeepSeek V4.1 Flash 1017 на `fireworks/us`. Ключ хранится
в `~/.ansa/openrouter/openrouter.key`, а сохранённые сессии привязаны к ключу.
Перед первым планом ansa проверяет строгую схему на выбранном маршруте.
Показанные в плане команды запускаются
после `Start`, каждая из корня отдельной копии кандидата с правами пользователя.

`/pr main` открывает черновик на GitHub для последней проверенной ветки
текущего разговора. Нужны `origin` на `github.com`, установленный `gh` с
доступом к репозиторию и проверенный кандидат на той же чистой базе, что на
GitHub. В публикацию входит новый коммит с проверенным деревом и публичным
родителем.

Интерфейс занимает всю высоту терминала по умолчанию: ввод закреплён внизу,
диалог расположен над ним. Текст переносится по словам с учётом ширины символов.
Ответы поддерживают базовое оформление Markdown; сообщения пользователя
сохраняют исходный текст. Модель, имя папки и активный режим видны внизу.
Вверху остаётся название открытого разговора и отметка при чтении истории.
Фон сохраняется из настроек терминала.

`/theme` открывает выбор оформления: Carbon, Pink, Red, Blue и Amber.
Стрелки показывают предпросмотр, Enter сохраняет выбор для всех проектов,
Esc возвращает прежнюю тему. Auto DMS подхватывает готовую палитру
DankMaterialShell и обновляется при её изменении.
Собственные темы загружаются из `~/.ansa/themes/*.json`, например:

```json
{"name":"Rose","colors":{"accent":"#dc8cd0"}}
```

Можно также задать `foreground`, `muted`, `selection`, `success`,
`warning`, `error`, `code` и `rule`. Пропущенные цвета берутся из Carbon.
Выбор хранится в `~/.ansa/ui.json`.

Диалог открывается без ожидания сети. После отправки задачи ansa проверяет вход
и модель, при необходимости обновляет токен. Порядок проверок описан
в [архитектуре](docs/architecture.md).

Диалог прокручивается колёсиком мыши. `PgUp`/`PgDn` также загружают страницы
истории. Во время чтения новые события и печать сохраняют позицию.
При выделении чата можно прокручивать историю с зажатой кнопкой мыши.
Удержание указателя у верхнего или нижнего края включает прокрутку.
Отпустите кнопку, чтобы скопировать выделенный текст.
После отправки сообщения по Enter чат возвращается вниз. Esc возвращает
к последним сообщениям. В дочернем разговоре следующий
Esc возвращает к родителю. При изменении ширины текст переносится заново,
поэтому верхняя строка может измениться.

`/` в начале слова открывает подсказки над вводом, в том числе посреди сообщения.
Стрелки выбирают команду. Tab или → дописывают выбранный вариант. Enter сразу
выполняет введённую целиком команду, например `/model`. Для неполного имени
Enter сначала дописывает его. `?` в пустой строке открывает помощь.
Команды внутри обычного текста передаются агенту как часть сообщения.

`↑` и `↓` выбирают предыдущие и следующие пользовательские сообщения текущей
сессии, включая историю после перезапуска. Возврат вниз восстанавливает черновик
и его курсор. Enter отправляет выбранное сообщение.
Печать и запоздалое обновление страницы не подменяют черновик. `Ctrl+J`
или распознаваемый терминалом `Shift+Enter` вставляет перенос. Bracketed paste
сохраняет текст до явного Enter. Alt+B/F перемещают курсор по словам.

Enter во время работы сохраняет уточнение и передаёт его модели после текущего
запроса или инструмента, до следующего действия. `Ctrl+C` очищает черновик.
При пустом вводе `Ctrl+C` прерывает текущий шаг и ускоряет доставку уточнения.
Когда чат свободен, два нажатия в течение двух секунд закрывают ansa.
Уточнения сохраняются в сессии. Если результат инструмента неизвестен или
запись в журнал не удалась, работа останавливается до выяснения причины.

Этап, таймер и индикатор работы находятся в диалоге рядом с текущей задачей.
Уточнение до доставки отмечено приглушённым маркером. `ANSA_NO_ANIMATION=1`
отключает анимацию. Счётчик токенов и заполнения контекста запланирован
в [TODO](docs/TODO.md).

У каждого вызова инструмента одна строка с файлом или командой и состоянием
работы. Чтение и поиск собраны в группы. Нажмите на строку, чтобы раскрыть
аргументы и вывод; `Ctrl+T` раскрывает все вызовы на странице. При перетаскивании
мышь выделяет текст для копирования. Ошибки и неизвестный результат
(`effect_unknown`) видны у свёрнутых групп. Подробности показывают аргументы и расшифрованный текст результата.
Короткий фрагмент раскрывается в чате, полный вывод открывается нажатием
«Весь вывод» в боковой панели. Esc возвращает прежнюю панель.
Время выполнения приблизительное и доступно в текущем процессе.
История вызовов и публичные сводки рассуждений загружаются из журнала сессии.

Правки через `edit_file` показывают имя файла, счётчики добавленных и удалённых
строк и цветной diff с номерами строк. Нажатие открывает полный diff этой правки:
`j/k` листают строки, `h/l` - длинные строки по горизонтали, `0/$` - к их краям,
`g/G` - к началу и концу, Esc возвращает в чат. Изменения команд и скриптов
видны через `/files` во вкладке Diff.

`Ctrl+G` выбирает результат в видимой части чата. Стрелки переходят между
строками, Enter открывает выбранный результат. Esc возвращает к строке с
прежним черновиком, повторный Esc возвращает к вводу. Из результата анализа
можно открыть график и исходный CSV. В diff клавиша `f` разворачивает панель
на всю ширину терминала.
`edit_file` доступен в новых сессиях; продолженная сессия сохраняет свой каталог
инструментов. Инструмент меняет одно точное вхождение текста или создаёт файл,
сохраняет результат в журнале и проверяет исходное содержимое перед записью.

При отпускании кнопки мыши ansa копирует текст и снимает выделение.
ANSA включает SGR mouse reporting в интерактивном режиме.
Сочетание для нативного выделения самого терминала зависит от его настроек.
`Ctrl+Y` или `/copy` отправляет весь подтверждённый диалог текущей
сессии в буфер обмена через OSC 52. Требуется поддержка копирования в терминале.
В копию входит подтверждённый текст диалога. Лимит - 64 KiB;
при превышении появляется сообщение об ошибке.
`Ctrl+Y` сохраняет черновик. Эталон интерфейса -
[финальный Carbon](designs/final-design/ux.md), с согласованными изменениями
в [TODO](docs/TODO.md#согласованный-carbon-ux).

В чате `Tab` переключает работу, планирование и оркестрацию. При открытой
панели `Tab` и `Shift+Tab` переводят фокус между ней и чатом. В shell `Tab`
дополняет команду. `/plan` переключает работу и планирование с инструментами
чтения. При возобновлении ansa сохраняет режим запроса. `/model` открывает выбор модели,
после выбора модели сразу открывается effort. Enter подтверждает пару настроек,
Esc возвращает к моделям. `/model NAME` также требует выбора effort.
`/effort` и `/thinking` отдельно меняют уровень рассуждения.
Выбор сохраняется для следующих запросов и новых запусков Carbon под тем же
аккаунтом. `/new` наследует модель и effort открытого разговора.
При возобновлении ansa восстанавливает выбор из журнала сессии.
Меню сразу использует кэш и обновляет каталог в фоне. Уровни берутся из каталога;
если они неизвестны, доступен выбор по умолчанию.

`/fork` создаёт независимую сессию с текущим подтверждённым контекстом и той
же рабочей папкой. `/btw вопрос` открывает отдельный разговор с доступом к файлам только для чтения.
В режиме оркестрации опишите задачу, например «Разберись, почему отчёт иногда
пустой». Просьба «Попроси субагентов проверить README» тоже открывает план.
`Start` подтверждает показанные шаги, `Refine` возвращает к вводу.
Координатор запускает до двух помощников одновременно, соблюдает зависимости
и составляет сводку из их результатов. `/agents` показывает задачи помощников.
В списке виден текущий шаг или последнее подтверждённое событие. Enter
открывает завершённый разговор, Esc возвращает к выбранной строке списка.
Во время выполнения можно сохранить черновик в поле и отправить его после
сводки. Рекомендации на будущее не мешают запуску плана на чтение.

Правки выполняются в отдельной копии текущих файлов. Ansa сверяет diff и
согласованные проверки содержимого, передаёт замороженные файлы ревьюеру и
показывает изменения ветки `ansa-result`. Оркестрация доступна в `--offline`
для проверки на фикстурах. В обычной сессии запуск требует маршрута модели
с подтверждённой поддержкой строгой схемы ответа.

Из чата можно открыть утверждённый план и исходный запрос. Рядом с результатом
правки показаны стороны сравнения и выполненные проверки. Enter на строке
проверки открывает её вывод. При открытии плана после перезапуска ansa
восстанавливает diff и доступные результаты проверок из сохранённых данных.

`/new` открывает пустой разговор в проекте. `/sessions` и `/resume` показывают
сохранённые разговоры. Доступные запросы и планы можно продолжить из этого
списка после сверки журналов. Ввод, курсор и позиция чтения сохраняются отдельно
для каждого открытого разговора.

`/files [путь]` открывает просмотр файлов: Source, Diff от Git
HEAD к текущему файлу и Markdown Preview с оформлением и переносами строк.
В списке файлов `j/k` перемещают курсор, `l` входит в каталог, `h` поднимается
к родителю, Enter открывает файл.
`s/d/p` выбирают вкладку, `j/k` листают,
`g/G` переходят к краям, `:номер` - к строке. `e` передаёт файл в `$EDITOR`
при отсутствии активной работы; ANSA возвращает терминал после выхода редактора.
Diff не запускает внешние diff/textconv helpers или lazy promisor fetch.
Без Git/HEAD либо при недоступном объекте Diff явно недоступен.

Отправьте задачу `Построй график продаж по месяцам из sales.csv и покажи его`.
Carbon откроет PNG из результата анализа рядом с чатом, если видны последние
сообщения. Готовый PNG можно открыть через `/files chart.png`. Агент в новой
сессии может открыть такой файл через `show_png`. Esc закрывает просмотр. Строка
«Открыть график» рядом с вызовом инструмента возвращает его из истории.
Kitty и Ghostty показывают изображение при прямом подключении. В других
терминалах панель показывает размер и путь к файлу. Tab и Enter копируют путь.

В `/files` клавиша `q` открывает shell в том же боковом окне и текущем каталоге.
Одна сессия shell сохраняется при переходах между файлами и терминалом.
В zsh Esc из строки приглашения возвращает файлы в текущем каталоге shell.
В Vim Esc остаётся клавишей редактора: `:q` возвращает shell, затем Esc - файлы.
Ctrl+C в терминале прерывает его программу. Ручные команды и их вывод остаются
в терминале. Внешний `e`/`$EDITOR` по-прежнему открывает выбранный файл.
При завершении ANSA очищает процессы принадлежащей shell сессии; процессы,
которые отделились через `setsid`, могут продолжить работу.

`--native-terminals` включает постоянный терминал агента в новом разговоре
Carbon. Агент сам запускает shell и использует его между сообщениями. Из
домашней папки укажите проект через `--cwd`. Терминал работает до закрытия
shell, выхода ansa или истечения лимита. Максимум 30 минут с запуска и 5 минут
без ввода агента.

Для локальной проверки запустите ansa с `--offline --native-terminals`.
`TERMINAL_DEMO` запускает тестовый shell, а `/terminals` показывает его экран.
Esc возвращает в чат.

Пока ответ поступает, он помечен как предварительный. Перед запуском инструментов
ansa проверяет весь ответ модели и все вызовы в нём. Итог подтверждается после
сохранения в журнале. При повторном открытии сессии история загружается
из подтверждённых записей.

`/exit` завершает процесс. Ctrl+D закрывает
ввод и даёт уже принятой работе закончиться. Ввод без TTY остаётся построчным;
для pipeline передавай одну задачу. Перенаправленный stdout и one-shot получают
проверенный итоговый ответ без интерактивного оформления.

Каждый запуск начинает новую сессию. `--continue` выбирает последнюю сессию
текущего каталога и режима live/offline; `--session` принимает её ID из стартового
сообщения. Журналы находятся в `~/.ansa/sessions/`. После аварийного завершения
незаконченный запрос можно продолжить из `/sessions`. Неизвестный результат инструмента блокирует
продолжение. `--offline` запускает демосессию с фиксированным ответом.

Конфигурация: `~/.ansa/config.json`, затем `<cwd>/.ansa/config.json`. Неизвестные поля и неверные типы отклоняются; `--print-config` показывает effective settings. Пример пользовательского файла:

```json
{"schema_version":1,"model":"gpt-6.1-sol","tool_rounds":32,"tool_calls":64,"process_timeout_ms":60000,"process_output_bytes":16384,"access_mode":"full_access","allow_sudo":false}
```

Проектный файл может менять model, returned_model, compact_bytes, tool_rounds, tool_calls, process_timeout_ms, process_output_bytes и exclude; security-поля разрешены только пользователю. Лимиты на сегмент Resume: до 64 раундов и 256 вызовов; такие же верхние границы действуют на весь turn. Команда ограничена десятью минутами и общим сохранённым stdout/stderr 16 KiB; обычный ненулевой exit возвращается модели как результат. Достигнутый лимит приостанавливает turn. При изменении effective config или начальных инструкций нужна новая сессия: сохранённый exact context не подменяется.

Инструкции загружаются из `~/.ansa/AGENTS.md` и `AGENTS.md` от корня до cwd. Вложенные инструкции выдаются tools до доступа к их каталогу; модель подтверждает актуальный instruction token. Автоматическая загрузка config/AGENTS не следует symlink/hardlink. Чтение файлов ограничено regular files, search имеет явные пределы и исключает вложенные instruction scopes до отдельного обращения. Dotenv, известные credential stores и собственные auth/session данные исключены из файловых tools. Произвольный shell работает с правами пользователя и не является файловой песочницей.

Full access действует без подтверждения каждой команды. Прямой sudo отклоняется; дочерние процессы получают `no_new_privs`, очищенные capabilities и stdin без терминала. Это запрещает повышение через setuid/file capabilities при exec, но не изолирует уже разрешённые пользователю привилегированные сервисы. Ask, интерактивный sudo и внешние hooks, включая TRASH, - в [TODO](docs/TODO.md).

ProcessRunner использует отдельный supervisor/subreaper: при Cancel, timeout и падении runtime он прекращает группу и усыновлённых потомков, сохраняет результат, затем освобождает ресурсы. При уничтожении самого supervisor результат считается неизвестным; убийство всех отделившихся потомков в этом случае не гарантируется без cgroup/container изоляции. Runtime не повторяет такой запуск автоматически.

## Сборка и проверка

Зависимости и компилятор зафиксированы через `flake.lock`. Проверенная платформа - NixOS/aarch64, ThinkPad X13s. В flake также объявлен x86_64-linux; он пока не проверен.

Production runtime написан на Rust. C++ sources/CMake удалены из текущей
сборки; проверенный rollback сохранён в tag `migration/m9-cutover` и Nix GC root
`~/Workspace/pet/ansa-baselines/m9-742d29c/cpp-package`.
`ansa` - имя Cargo binary и default Nix package; `.#ansa-rust` и alias `ansa-rs`
сохранены для совместимости. Обычные v1 records и codecs
сохраняются; новая ветка с наследуемым контекстом использует атомарный `init`
версии 2 с полным fork seed. Старый reader обязан отказать на такой сессии.

```sh
nix develop --no-update-lock-file -c cargo test --manifest-path rust/Cargo.toml --workspace --locked
nix develop --no-update-lock-file -c cargo fmt --manifest-path rust/Cargo.toml --all -- --check
nix develop --no-update-lock-file -c cargo clippy --manifest-path rust/Cargo.toml \
  --workspace --all-targets --all-features --locked -- -D warnings
nix flake check --no-update-lock-file
nix build --no-update-lock-file
./result/bin/ansa --self-check
./result/bin/ansa --offline-demo
```

Полный обязательный inventory и границы - [M10](docs/migration/M10.md).
Native tests проверяют protocol/admission/replay, v1 journal/auth, crash/recovery,
`effect_unknown`, адресную отмену, process cleanup и frontend. Дополнительные
installed CLI gates используют только synthetic profiles и собственные процессы:

```sh
nix develop --no-update-lock-file -c cargo build --manifest-path rust/Cargo.toml \
  --workspace --all-targets --all-features --locked
nix develop --no-update-lock-file -c cargo build --manifest-path rust/Cargo.toml \
  --release --example frontend_adapter --features frontend-test-adapter --locked -j 2
nix develop --no-update-lock-file -c python -B tests/rust_acceptance.py \
  --binary ./result/bin/ansa --fixture-binary rust/target/release/examples/frontend_adapter \
  --output /tmp/ansa-rust-acceptance
nix develop --no-update-lock-file -c python -B tests/cli_progress_pty.py \
  --binary ./result/bin/ansa --fixture-binary rust/target/release/examples/frontend_adapter \
  --output /tmp/ansa-pty-check
nix develop --no-update-lock-file -c python -B tests/carbon_workspace_pty.py \
  --binary ./result/bin/ansa --output /tmp/ansa-workspace-check
nix develop --no-update-lock-file -c python -B tests/carbon_visual_pty.py \
  --binary ./result/bin/ansa --fixture-binary rust/target/release/examples/frontend_adapter \
  --output /tmp/ansa-visual-check
nix develop --no-update-lock-file -c python -B tests/editor_setup_pty.py \
  --binary ./result/bin/ansa --output /tmp/ansa-editor-setup-check
nix develop --no-update-lock-file -c python -B tests/cli_output_drain.py \
  --fixture-binary rust/target/debug/examples/frontend_adapter --output /tmp/ansa-drain-check
nix develop --no-update-lock-file -c python -B tests/cli_live_startup.py \
  --binary ./result/bin/ansa --output /tmp/ansa-startup-check --repeat 256
nix develop --no-update-lock-file -c python -B tests/replay_mutation.py \
  --output /tmp/ansa-replay-mutations
nix develop --no-update-lock-file .#quality -c python -B tests/parity/quality.py \
  --mode address --output /tmp/ansa-rust-asan
```

Каталоги evidence должны быть новыми, code checkpoint чистым. Сначала закончить
сборки и replay mutants, затем выполнять чувствительные к времени PTY/fault gates.
Carbon workspace проверяется в 20 production PTY-сценариях и двух проверках
заблокированного вывода при передаче терминала редактору. Возврат после редактора
восстанавливает основной экран, курсор, стили, termios и флаги дескрипторов.
`tests/carbon_visual_pty.py` добавляет три цветных сценария в обоих профилях:
инструменты и выделение, сохранение тем с Auto DMS, редактирование с inline diff.
Три контрольных примера проверяют VT oracle цветов и подложек.
ASan использует pinned nightly с instrumented std и leak detection.
Этот toolchain не поддерживает Rust UBSan.

`tests/parity/` сохраняет historical differential harness и C++ assertions как
reference. Старый общий Python/CTest gate требует pre-M10 checkout и frozen
C++ binaries; он не входит в текущий Rust-only inventory. Для воспроизведения
старой миграционной матрицы используй её tag и документированные prerequisites.
Текущие legacy golden readers не запускают C++.

## Вход и live JSONL

```sh
./result/bin/ansa --login
./result/bin/ansa --auth-status
./result/bin/ansa --models
./result/bin/ansa --live-jsonl --session .omh-live-session --model gpt-6.1-sol
```

Собственная регистрация использует браузер, PKCE S256, state и nonce. Если Firefox предлагает выбрать профиль, выбери его и заверши вход. CLI также печатает ссылку для ручного открытия в браузере на той же машине. Callback слушает только `127.0.0.1` и ждёт до пяти минут. Для повторного запроса разрешения использовать ChatGPT plan есть `--login-consent`.

Lifecycle следует официальным контрактам [регистрации и входа](https://developers.openai.com/siwc/token-sharing-open-source/sign-in), [профилей и сессий](https://developers.openai.com/siwc/token-sharing-open-source/profiles-and-sessions) и [токенов](https://developers.openai.com/siwc/token-sharing-open-source/token-reference).

Новый профиль хранится в `~/.ansa/auth`. Если уже существует прежний собственный каталог `$XDG_STATE_HOME/oh-my-harness` или `~/.local/state/oh-my-harness`, ansa продолжает использовать его с сохранённым входом. Каталог имеет права `0700`, файлы - `0600`. Токены хранятся в открытом виде отдельно от журнала и исходников. `--auth-dir /absolute/path` выбирает другой собственный каталог для любой auth-команды или live CLI. `--auth-status` выводит статус, хеш привязки учётной записи и время до истечения и обновления токена.

Live JSONL использует команды из таблицы ниже и `session: "live"` из начального snapshot. `.omh-live-session/` исключён из Git; для offline сессии используется отдельный каталог. Каталог session связывается с выбранными account, requested/returned model и context policy. Обычный новый Submit получает сохранённую историю; смена binding требует новой сессии. `--returned-model EXACT` задаёт точную policy, если сервер возвращает имя, отличающееся от выбранного slug. По умолчанию обе строки равны.

`--compact-bytes N` задаёт запасной порог JSON, если размер окна модели неизвестен (по умолчанию 262144). Подробнее в разделе [Autocompact](#autocompact).

```sh
./result/bin/ansa --refresh-auth
./result/bin/ansa --logout
```

Вход сохраняется между запусками. Rotating refresh сериализуется файловой блокировкой и выполняется перед истечением access token. Явный `--refresh-auth` также соблюдает серверный `earliest_refresh_at`; до этого времени он возвращает `auth_refresh_not_ready`. При неопределённом результате обмена токен не используется повторно, требуется новый вход. Logout немедленно запрещает новый dispatch, удаляет локальные токены и пытается отозвать refresh token. Поле `remote_revocation_confirmed` сообщает результат. Сеть недоступна, неизвестен результат предыдущего обмена или повторный вход заменил ещё активную сессию - подтверждение полного отзыва остаётся `false`. Забытые сессии можно отозвать через настройки ChatGPT.

При сетевой ошибке turn остаётся в `paused`; после устранения причины можно отправить Resume с его `request_id`. После logout и повторного входа Resume явно принимает новую auth-сессию того же аккаунта. Известные outcomes tools сохраняются, незапущенный хвост допускает безопасную повторную попытку. Logout передаёт stop активному executor и ждёт освобождения его auth lease. Legacy JSONL сохраняет pure `get_test_value`; обычный CLI предоставляет локальные tools.

## Команды JSONL

`ansa --offline-jsonl` читает одну команду на строку и отдаёт события в stdout. Первый snapshot сообщает `session` (`offline`), `incarnation` и `stream_sequence`. В каждой команде передай `schema_version: 1`, эти `session` и `incarnation`, а также `turn`, `command_id`, `type` и поля payload.

| `type` | Поле payload | Поведение |
|---|---|---|
| `submit` | `text`, необязательный `mode` | Режим basic по умолчанию или plan. Ставит turn в FIFO. `command_accepted` сообщает его `request_id`. Повтор той же команды даёт `command_replayed`; другой текст или mode с тем же ID даёт ошибку. |
| `cancel` | `target` | Используй `request_id` активного turn. Отмена проходит и при заполненном FIFO. |
| `resume` | `target` | Используй `request_id` приостановленного turn и новый `command_id`. Продолжение начинается после сохранённой безопасной границы. |
| `snapshot` | `after_sequence` | Передай последний известный cursor или `0`. Ответ содержит актуальное состояние turn и `through_sequence`. |
| `transcript` | `before`, `limit`, необязательный `forward` | До 64 записей и 256 KiB текста. По умолчанию читает назад до exclusive before, 0 означает конец. При forward:true читает вперёд начиная с before, 0 означает начало. Ответ возвращает begin/end/total, entries и исходный query. |

При разрыве `stream_sequence` или `resync_required` запроси snapshot и продолжи чтение после `through_sequence`. Читай stderr отдельно от событий. Туда идут ошибки разбора и постановки команды в очередь. EOF закрывает ввод и даёт runtime завершить текущую работу. Приостановленный turn остаётся ждать Resume.

Snapshot дополнительно содержит mode, transcript_size и nullable preview с generation/text/state. Событие text_preview имеет те же поля; state принимает preview/committed/discarded. Legacy text_delta сохраняет полный подтверждённый финальный ответ. Проекция transcript содержит только пользовательский текст и durable итоговые ответы. JSONL не включает предварительный streaming по умолчанию; эти типы общие с интерактивным frontend.

## Сохранённая сессия

```sh
./result/bin/ansa --offline-jsonl --session .omh-session
```

Каталог создаётся с правами `0700`, journal parts - `0600`. `.omh-session/` исключён из Git. Повторный запуск с тем же каталогом восстанавливает сессию и выдаёт новую runtime incarnation. Перенеси её из начального snapshot в команды текущего подключения. Незавершённый turn получает состояние `paused` и ждёт явный `resume`. Поля `request_id`, `turn` и `resume_available` доступны в snapshot.

Журнал сохраняет историю завершённых turns, полные provider-native items, контекст и результаты tools. `command_accepted` и `done` публикуются после `fsync`. Постановка команды в FIFO сама по себе сохраняет её только в памяти. Durable Submit IDs дедуплицируются между перезапусками. Другой процесс получает `session_busy`.

Перед executor сохраняется intent, после него - outcome. Outcome синхронизируется до следующего executor. При intent без outcome ProcessRunner проверяет сохранённое свидетельство результата supervisor и синхронизирует outcome в canonical journal. Если достоверного результата нет, `effect_unknown` блокирует Resume; повторного запуска команды нет. При ошибке записи доступны snapshot и Cancel, дальнейший dispatch останавливается. Snapshot помечает такое состояние через `storage_failed` и `durable:false`, включая известные результаты, которые не удалось сохранить.

Оборванный хвост остаётся в исходном файле, запись продолжается в следующей связанной части. Повреждение проверенной записи переводит сессию в режим просмотра с диагностикой.

## Autocompact

Перед новым запросом ansa проверяет, занята ли половина окна модели.
При окне 1 000 000 токенов порог составляет 500 000. К замеру провайдера
ansa прибавляет оценку нового сообщения. Если подходящего замера нет,
ansa оценивает токены всего запроса, включая инструкции и инструменты.
Размер окна сохраняется в журнале. При неизвестном окне действует порог
`compact_bytes` (256 КиБ по умолчанию). Отдельные проверки учитывают
лимит элементов контекста и предельный размер запроса OpenRouter.

Autocompact сохраняет сводку ранней части разговора и два последних
завершённых запроса с уточнениями. Полная история доступна в журнале.
После ошибки autocompact запрос можно продолжить из `/sessions`.

## Границы реализации

Команды интеграции с Neovim описаны в [ansa.nvim](integrations/ansa.nvim/README.md),
протокол - в [дизайне рабочей среды](docs/TERMINAL_WORKSPACE_DESIGN.md).

- HTTPS bytes обслуживает Hyper/rustls; bounded SSE framer отвечает за события, Responses reducer - за semantic admission.
- Admission требует successful terminal, согласованных IDs/model/items/arguments и завершённого framing. До проверки всего batch executors недоступны.
- Owning JSON сохраняет порядок native items, opaque strings и числовые tokens без округления. Replay использует исходные prefix, context epoch и advertised tool snapshot.
- `ToolRegistry` хранит compiled descriptor/schema/validator/executor/revision. Provider получает описания инструментов. Обычный CLI предоставляет файлы, команды, workspace/выделение и CSV-анализ; продолжение сессии сохраняет её прежний каталог. Legacy JSONL сохраняет pure snapshot.
- Один control owner ведёт состояние turn. Worker выполняет mock generation, подготовку и dispatch tool и строит continuation. Commands/events v1 и snapshot/resync общие для JSONL и текстового frontend.

CLI предоставляет self-check, offline round trip и live SIWC. Mock строит финальный response из фактического результата tool в continuation.

Сетевой adapter проверяет TLS, ограничивает размер и длительность transfer и не следует redirects. Политика MIME по умолчанию строгая; отсутствие MIME разрешается явно только для точного `POST https://api.openai.com/v1/responses`, `stream:true`, `store:false`, HTTP 200. Неверный или повторный MIME всегда приводит к отказу. Production live путь подаёт chunks в общий SSE reducer во время transfer. Preview предварительный; `CompletedResponse` появляется только после successful transfer, terminal и EOF. Тестовый injectable buffered adapter сохраняется.

Поддерживается закрытое подмножество Responses events: lifecycle, function arguments, message text/refusal, reasoning summary и неисполняемый `response.metadata` с явным matching response ID. Неизвестные events/items отклоняются. Требуются `response.created`, непрерывные unsigned sequence numbers с нуля и точное совпадение returned model с явно выбранной policy. Для окончательного reasoning item обязательно непустое `encrypted_content`; промежуточное значение added может быть неполным.

Для нового pure tool достаточно `ToolDefinition` и регистрации; пример с аргументами есть в suite `tools`. Поддерживаемая локальная schema: object/array/string/integer/number/boolean/null, все properties обязательны, `additionalProperties:false`; domain validation задаётся compiled validator. Неизвестные schema keywords отклоняются. При изменении реализации нужно менять `implementation_revision`.

Архитектура и ограничения: [docs/architecture.md](docs/architecture.md). Фактические gates: [docs/implementation-report.md](docs/implementation-report.md).

Request profile следует [официальному SIWC Preview contract](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations) и [models/inference contract](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference), сверенным 2026-11-11.
