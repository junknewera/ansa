# M0: offline capture и проверка evidence

`run.py` использует только Python standard library. Он записывает один C++ baseline и проверяет его фактические результаты. Runtime Rust, differential comparison, fault injection и performance runner в M0 не реализованы. Команды `compare`, `fault` и `perf` возвращают код **2** с явным `not implemented`; неизвестный milestone тоже не проходит.

## Команды

Все native gates выполняются на NixOS/aarch64 X13s внутри pinned `nix develop --no-update-lock-file`. На другой платформе можно собирать и исследовать evidence, но это не закрывает native gate. Существующий production executable остаётся C++.

Сначала собрать baseline и отдельный test-only Linux probe:

```sh
nix develop --no-update-lock-file
cmake -S . -B build-ansa -G Ninja -DCMAKE_BUILD_TYPE=Debug
cmake --build build-ansa -j 4
python3 tests/parity/run.py preflight --probe build-ansa/ansa_preflight --output build-migration/preflight.json
python3 tests/parity/run.py capture --cpp-build build-ansa --output build-migration/cpp-debug --run-ctest
python3 tests/parity/run.py check --milestone M0 --evidence build-migration/cpp-debug
```

`capture` сам собирает отдельные Linux и toolchain preflight результаты. Неудача одного не отменяет второй. Standalone `preflight` удобен до полного CTest: Linux probe проверяет реальные syscalls и принадлежащие ему temporary files/children; `check_toolchain.py` проверяет declarations и фактически запущенные rustc/Cargo. Отсутствующие возможности или tools сохраняются как failure/unsupported, а не PASS. Probe не входит в CTest, поэтому не меняет baseline 87 cases.

**`--run-ctest` обязателен для evidence gate.** Без флага capture запускает только `ansa --self-check`, `ansa --offline-demo`, version queries и CTest inventory. Он записывает `ctest.status = NOT_RUN`; `check` отвергает такой bundle как незавершённый. С флагом выполняется полный `ctest --output-on-failure --no-tests=error --output-junit ...`, без фильтра исключений. Неудачный case остаётся в результатах. Harness не собирает и не исправляет C++ автоматически.

Для sanitizer profile:

```sh
cmake -S . -B build-ansa-sanitize -G Ninja -DCMAKE_BUILD_TYPE=Debug -DOMH_SANITIZERS=ON -DOMH_MUTATION_CHILD=ON
cmake --build build-ansa-sanitize -j 2
ASAN_OPTIONS=detect_leaks=1 UBSAN_OPTIONS=halt_on_error=1 python3 tests/parity/run.py capture --cpp-build build-ansa-sanitize --output build-migration/cpp-sanitize --run-ctest
python3 tests/parity/run.py check --milestone M0 --evidence build-migration/cpp-sanitize
```

Manifest перечисляет все **87** зарегистрированных baseline IDs. `OMH_MUTATION_CHILD=ON` исключает только `mutation-drop` и `mutation-reorder`, оставляя **85**; `check` допускает такой набор только для `OMH_SANITIZERS=ON`. Пустая registration, missing/duplicate ID, skipped/disabled case, timeout, ошибка CTest или неисполненный mutant не проходят проверку. Nix Release 87/87 и сохранение executable/Nix closure остаются отдельными gates: сохранить output/log `nix build --no-update-lock-file` вместе с checkpoint. Одного Debug bundle для M0 недостаточно.

`--output` capture — **новая** директория. Standalone preflight требует новый JSON path и создаёт рядом `<имя>.logs/`. Повторный запуск не перезаписывает evidence предыдущего запуска. `--probe` можно указать явно; default capture использует `<cpp-build>/ansa_preflight`, standalone preflight — `build-ansa/ansa_preflight`.

## Что сохранено и что проверяется

`capture.json` хранит:

- HEAD и состояние worktree, SHA-256 каждого code/build/fixture input, digest manifest; dirty snapshot остаётся явно dirty.
- Platform/kernel/OS, фактические CMake source/profile/options, SHA-256 C++ executable/test binaries, dependency versions из `--self-check`, compiler/CMake/CTest versions и отдельный Rust toolchain report.
- Actual argv, synthetic cwd, exit code, timeout и elapsed time каждого запуска. Stdout и stderr сохраняются **отдельными файлами без изменения байтов**, с SHA-256. Значения environment не дампятся; записываются только `ASAN_OPTIONS` и `UBSAN_OPTIONS` для sanitizer gate.
- Фактическая CTest registration из `--show-only=json-v1`, ожидаемые manifest IDs и при `--run-ctest` raw JUnit с hash и разбором каждого case. В JUnit CTest объединяет вывод отдельного теста; harness не выдаёт этот combined stream за отдельные stdout/stderr теста.

`check` сверяет текущие inputs и binary hashes, actual raw outputs, exact case IDs, CLI semantic assertions, JUnit failures/skips, Linux/toolchain результаты и native platform. Exit 0 означает только `PASS_FOR_THIS_CAPTURE`. Это **не M0 GO**: command отдельно перечисляет manual obligations для X13s hardware identity, всей Debug/Release/sanitizer matrix, Nix/Rust build/fmt/clippy/tests, сохранённого baseline и independent review/checkpoint. Происхождение binary из конкретных source bytes подтверждается записанной clean build/Nix workflow и review; один hash этого не доказывает. После изменения исходников или пересборки нужен новый capture.

Capture возвращает nonzero при неудаче выполненных проверок и всё равно сохраняет их evidence. Успешная запись без `--run-ctest` не означает успешный test gate. Некорректный input, missing files и неподдерживаемые команды также возвращают nonzero.

## Isolation и пределы M0

Harness не читает credential stores, не вызывает auth/login/live API и не запускает обычный agent bootstrap. `--self-check` и `--offline-demo` выполняются из собственной temporary cwd; существующие CTest cases используют synthetic stores/fixtures. `HOME` текущей сессии не переназначается. `--offline --cwd TEMP --auth-dir TEMP` не используется: эти параметры не изолируют global config/AGENTS.

При timeout harness завершает и reap-ит запущенный им process group. Cleanup detached descendants проверяют существующие process tests и Linux probe; kill process group сам по себе не доказывает cleanup после `setsid`. Если proc children недоступен, capability gate остаётся blocked. Harness не ищет и не сигналит произвольным процессам. На текущем ограниченном x86_64 host отсутствие proc children нельзя выдавать за проверку X13s.

`perf_budget.json` содержит **null/pending**, пока нет повторяемого native baseline. Записанный `elapsed_seconds` — диагностическое время одного запуска, не latency budget и не benchmark. Production first frame остаётся PENDING до M7/M8 с контролируемым synthetic filesystem/OS environment; scaffold и safe CLI не измеряют production bootstrap.

Проверки самого harness:

```sh
python3 -m unittest discover -s tests/parity -p 'test_*.py' -v
git diff --check
```

Новые harness tests не регистрируются как CTest и не увеличивают число baseline cases.
