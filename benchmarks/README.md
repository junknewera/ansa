# Бенчмарк сессий ANSA

Стенд запускает обычный Carbon через PTY. В манифесте каждая конфигурация получает собственные executable, режим, маршрут, модель, effort, инструкции и при необходимости настройки MCP. Один и тот же манифест подходит для сравнения двух сборок, моделей, режимов или вариантов инструкций. Для оценки одной правки у A и B меняется только соответствующее поле.

| Режим | Что получает проверяющий |
| --- | --- |
| `work` | Ответы и рабочую копию после исполнения. |
| `plan` | Ответы из штатного Plan с инструментами чтения и рабочую копию для проверки отсутствия правок. |
| `planner` | Структурированный черновик плана Orchestrate. Стенд закрывает диалог подтверждения. |
| `orchestrate` | План, итоговую сводку и чистый checkout выпущенного `ansa-result` для задач с записью. |
| `orchestrate+` | Те же артефакты и журнал вызовов с моделями ролей strong, medium и weak. |

В Work и Plan можно передать несколько реплик. Для трёх режимов с составлением плана задаётся одна реплика. Для оценки другого встроенного промпта, например ревью или свёртки, укажите два бинарника, собранных с нужными вариантами промпта. Поле `guidance` подаёт инструкции как пользовательский `AGENTS.md` внутри временного профиля.

## Офлайн-проверка

```sh
nix develop -c cargo build --locked --manifest-path rust/Cargo.toml \
  -p ansa --bin ansa --example frontend_adapter --features frontend-test-adapter
nix develop -c python -B benchmarks/smoke.py --binary rust/target/debug/ansa \
  --frontend-adapter rust/target/debug/examples/frontend_adapter \
  --output /tmp/ansa-benchmark-smoke
```

Smoke проверяет пять режимов, три модели, многоходовую сессию и выдачу кандидатов обоих режимов оркестрации на синтетическом транспорте. Каталог вывода должен быть новым.

## Публичная задача Terminal-Bench

Подготовка скачивает закреплённые инструкцию, генератор логов и авторские тесты задачи `log-summary-date-ranges`. Адаптер заменяет `/app` на рабочий каталог сессии. Получаются исходные данные и готовый манифест для A/B пользовательских инструкций. [Исходная задача](https://github.com/harbor-framework/terminal-bench-2/tree/2fd12b88aafdd04a52c298e3940bcb189f9766d6/log-summary-date-ranges)

```sh
nix develop -c python -B benchmarks/prepare_terminal_logs.py \
  --output /tmp/ansa-public-logs --binary result/bin/ansa \
  --route siwc --model gpt-6.1-sol --auth-dir "$BENCH_AUTH"
nix develop -c python -B benchmarks/bench.py /tmp/ansa-public-logs/manifest.json
nix develop -c python -B benchmarks/bench.py /tmp/ansa-public-logs/manifest.json \
  --output /tmp/ansa-public-logs-evidence --live
```

`BENCH_AUTH` указывает на отдельный профиль бенчмарка, уже авторизованный штатной командой ANSA. Запуск без `--output` печатает только расписание, настройки и хеши. `--live` разрешает запросы к модели. Стенд даёт каждой попытке свежие HOME, XDG, Git-репозиторий и сессию. Для живых прогонов используйте отдельного пользователя или контейнер, где агенту доступны ровно нужные данные и инструменты. Shell работает с правами запустившего его пользователя. Секреты остаются в профиле для ANSA. Маршрут OpenRouter принимает модели, которые поддерживает указанный бинарник. Текущая production-сборка закрепляет основную модель OpenRouter. SIWC принимает доступные аккаунту модели.

В созданном манифесте можно заменить `mode` у варианта на `orchestrate` или `orchestrate+`. Для Orchestrate+ добавьте модели ролей. Для других публичных наборов подайте подготовленные входы и независимый проверяющий скрипт. Для восьми задач из #73 используйте общий подготовщик ниже.

## Публичные задачи из #73

[План реализации и критерии приёмки](ALL_AROUND.md) описывают все восемь задач. Подготовщик работает без обращения к модели. Он скачивает исходные данные выбранных задач и записывает манифест после проверки всех путей. Укажите свежий каталог и существующий executable. Для подготовки всех восьми задач опустите `--task`.

```sh
python3 -B benchmarks/prepare_public.py --list
python3 -B benchmarks/prepare_public.py --task intercode-bash-fs1-48 --task mteb-retrieve \
  --output /tmp/ansa-public-ab --binary result/bin/ansa --model gpt-6.1-sol \
  --route siwc --auth-dir "$BENCH_AUTH" --candidate-guidance ./candidate.md
manifest=/tmp/ansa-public-ab/manifest.json
python3 -B benchmarks/bench.py "$manifest" > /tmp/ansa-dev-plan.json
python3 -B benchmarks/bench.py "$manifest" --split holdout > /tmp/ansa-holdout-plan.json
python3 -B benchmarks/bench.py "$manifest" --frozen-plan /tmp/ansa-dev-plan.json \
  --output /tmp/ansa-ab-dev --live
python3 -B benchmarks/bench.py "$manifest" --split holdout --frozen-plan /tmp/ansa-holdout-plan.json \
  --output /tmp/ansa-ab-holdout --live
```

В примере `candidate.md` — ваш файл с инструкциями для варианта B. Без `--candidate-guidance` создаётся один вариант Work. Строгое сравнение требует задач с контрольными примерами в обеих выборках. Работа с другим фактором и формат отчёта описаны в [COMPARISON.md](COMPARISON.md).

Для Git нужен клиент Git, для декомпиляции — нативный C-компилятор. Для подготовки SWE-bench нужен `pyarrow`, для оценки патча — Docker и закреплённый SWE-bench harness. Подробности в [описании двух задач с изображениями](IMAGE_TASKS.md). Исходные ревизии и зависимости конкретных задач зафиксированы в адаптерах. Оригиналы изображений хранятся вне рабочего каталога агента. Carbon пока не умеет передавать изображение в пользовательской реплике. Для такой задачи стенд записывает `unsupported` с режимом и моделью, пропускает обращение к провайдеру и исключает пару из числа побед.

## Манифест

- `version` равен 1. `seed` задаёт порядок запуска, `repetitions` задаёт число повторов каждой пары задача × конфигурация.
- `arms` содержит записи `id`, `binary`, `mode`, `route`, `model`. `route` принимает `offline`, `siwc`, `openrouter`. `effort` принимает уровень рассуждений или `null` для штатного значения. Для `orchestrate+` обязателен объект `roles` с записями `strong`, `medium` и `weak`. В каждой записи задаются `model` и при необходимости `effort`. `guidance` и `mcp_config` указывают на текст инструкций и JSON конфигурации MCP. Для живых маршрутов обязателен `auth_dir`.
- `tasks` содержит записи `id`, `turns`, `modes`, `workspace` и `verifier`. `modes` задаёт совместимые режимы. `workspace` указывает на каталог исходных файлов для нового чистого Git-репозитория. `verifier` получает рабочий каталог и `answer.json` двумя аргументами. В JSON есть `answers`, `plan` и `candidate`. Код 0 означает успех, 1 означает ошибку результата, остальные коды означают поломку стенда. `verifier_inputs` перечисляет дополнительные файлы оценщика для проверки хешей. `source` сохраняет происхождение задачи. Пути разрешаются относительно манифеста.
- `budget` задаёт `trial_seconds`, `verifier_seconds`, `total_seconds`, а также лимиты `tool_rounds` и `tool_calls` на turn ANSA. Лимиты распространяются на выбранный runtime. Расход собственно модели измеряется по сохранённым ответам. Неудачные сетевые попытки могут не попасть в журнал.

`report.json` показывает результаты и парное сравнение на совпадающих задачах. `trial.json` сохраняет модели из журнала, число подтверждённых ответов, исполненных инструментов, токены при полной отчётности провайдера и время. В каталоге попытки также остаются ANSA-журналы, запись PTY и вывод verifier. Эти данные могут содержать текст заданий и ответы модели.
