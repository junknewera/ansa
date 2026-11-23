# Задачи с изображениями

| ID | Вход | Проверка |
| --- | --- | --- |
| `charxiv-reasoning-0` | Оригинальный JPEG фигуры 0 из CharXiv val и вопрос о падении точности между двумя сессиями | Запрос требует только имя модели. Сравнение с ответом CharXiv допускает изменение регистра и конечную точку. |
| `swebench-chartjs-8567` | Исходники Chart.js на базовом коммите, сообщение из dev split и оригинальный PNG со скриншотом | Адаптер извлекает патч относительно базовой версии и передаёт его официальному SWE-bench с тестами задачи и тремя эталонными PNG. |

Подготовщик сохраняет изображения вне рабочего дерева и связывает каждый файл с первой репликой через `attachments[].turn = 0`. В Carbon команда `/attach PATH` прикладывает PNG или JPEG к следующему сообщению в Work или Plan. Для отмены используйте `/attach --clear`. На реплику допускается до четырёх изображений суммарно до 2 МиБ. Для бенчмарка укажите `--image-input` при подготовке варианта с поддержкой изображений. CharXiv допускает Work и Plan, правка Chart.js требует Work. Режимы оркестрации пока принимают только текст.

## Подготовка

В Python-окружении подготовщика для SWE-bench нужен `pyarrow` (проверено с 18.1.0). CharXiv использует стандартную библиотеку Python и загружает архив изображений размером 141 МБ. Подготовщик проверяет SHA-256 файлов. Архив Chart.js, изображения для проверки и авторский контрольный патч остаются в каталоге оператора.

```sh
python -B benchmarks/prepare_public.py --task charxiv-reasoning-0 --task swebench-chartjs-8567 \
  --output /tmp/ansa-image-tasks --model offline --binary result/bin/ansa
python -B benchmarks/bench.py /tmp/ansa-image-tasks/manifest.json
```

## Проверка оценщиков

У каждого адаптера есть контрольные исполняемые файлы `operator/control.py` и `operator/verify.py`. Контрпример CharXiv содержит отрицание правильного ответа. Для Chart.js положительный контроль применяет исходный авторский патч, а отрицательный меняет только README. Строка SWE-bench задаёт одну проверку `FAIL_TO_PASS` для `core.layouts/long-labels.js` и пустой список `PASS_TO_PASS`. Адаптер запускает модуль `core.layouts` в Chrome внутри закреплённого образа и требует в журнале Karma явный результат целевой проверки. Для воспроизведения контроля укажите пустой рабочий каталог и путь к ответу вне него.

```sh
task=/tmp/ansa-image-tasks/charxiv-reasoning-0
control_dir=$(mktemp -d)
mkdir "$control_dir/workspace"
"$task/operator/control.py" pass "$control_dir/workspace" "$control_dir/answer.json"
"$task/operator/verify.py" "$control_dir/workspace" "$control_dir/answer.json"
```

Оценка Chart.js требует Docker с поддержкой linux/amd64 и пакета SWE-bench, установленного в Python подготовщика на закреплённой ревизии. Образ тестовой среды закреплён по digest. Подготовка входов работает без Docker.

```sh
python -m pip install 'swebench @ git+https://github.com/SWE-bench/SWE-bench.git@02e7a74ffd0b707aab73d203fe87bdc7c76afc8e'
```

Проверяющий сохраняет предсказание, имя контейнера и журналы SWE-bench в `swe-evaluation/` рядом с ответом. Официальный harness удаляет контейнер при штатном завершении. После внешнего принудительного прерывания имя для очистки находится в `swe-evaluation/container-name`. Отсутствующее или изменённое изображение теста, отсутствие отчёта или ошибка среды дают код 2. Корректный патч получает 0, ошибка решения — 1.

На Ubuntu с Docker для обоих контролей можно запустить одну команду. Она проверяет, что официальный отчёт относит один и тот же `FAIL_TO_PASS` к успешным на авторской правке и к неуспешным на изменении README. Итог появляется в `results.json`, а результаты Karma — в каталогах каждого контроля.

```sh
python -B benchmarks/check_chartjs_oracle.py \
  /tmp/ansa-image-tasks/swebench-chartjs-8567/operator /tmp/ansa-chartjs-controls
```

[Задача GitHub Actions](../.github/workflows/chartjs-oracle.yml) устанавливает зависимости из `uv.lock` официального оценщика и сохраняет отчёты в артефакте `public-benchmark` (каталог `controls/swebench-chartjs-8567`).
