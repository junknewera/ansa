# Offline capture и проверка evidence

`run.py` использует только Python standard library. M0 capture сохраняет C++ baseline. M1 добавляет `compare/fault --suite risk` и `check --milestone M1`; M2 - `--suite protocol` и `--milestone M2`; M3 - `--suite auth` и `--milestone M3`. Другие future suites, `compare/fault` без suite и `perf` возвращают nonzero с явным `not implemented`; неизвестный milestone тоже не проходит. Production runtime остаётся C++.

## M3 auth suite

M4 добавляет `compare/fault --suite sessions` и `check --milestone M4`.
Собери `ansa_session_oracle` и `cargo build --manifest-path rust/Cargo.toml
--locked --example session_adapter --features session-test-adapter`, затем
выполни capture в новые directories. Commands и scope - в
[M4 report](../../docs/migration/M4.md). Existing C++ journals используются
только в виде synthetic fixtures.

В pinned Nix shell собери оба adapters и выполни:

```sh
cmake --build build-m3 --target ansa_auth_oracle -j 2
cargo build --manifest-path rust/Cargo.toml --locked -p ansa --example auth_adapter --features auth-test-adapter
python tests/parity/run.py compare --suite auth --cpp-build build-m3 --rust-target rust/target --output build-migration/M3
python tests/parity/run.py fault --suite auth --cpp-build build-m3 --rust-target rust/target --output build-migration/M3-fault
python tests/parity/run.py check --milestone M3 --evidence build-migration/M3
```

Матрицы требуют 220 compare и 273 fault cases: независимые JWT/claims assertions,
все 16 native auth suites, совместимость private store в обе стороны,
21 точку падения с восстановлением другим бинарником, file guards и настоящий
межпроцессный refresh. Fixture keys и tokens синтетические; adapters работают
только с принадлежащими тесту temporary roots. Checker повторно проверяет raw
commands, snapshots и signed bytes. Пять reviewed stricter corrections описаны
в [M3 report](../../docs/migration/M3.md).

Для всех harness regressions нужны также `ansa_protocol_oracle`, `ansa_risk_oracle`
и все Rust examples/bins с features `protocol-test-adapter,auth-test-adapter`:

```sh
ANSA_PARITY_CPP_BUILD=build-m3 python -m unittest discover -s tests/parity -p 'test_*.py'
```

Переменная задаёт build directory для protocol regression tests; default - `build-m2`.

## M2 protocol suite

Собери `ansa_protocol_oracle` и Rust example с feature `protocol-test-adapter`:

```sh
cmake --build build-ansa --target ansa_protocol_oracle -j 2
cargo build --manifest-path rust/Cargo.toml --locked -p ansa --example protocol_adapter --features protocol-test-adapter
python3 tests/parity/run.py compare --suite protocol --cpp-build build-ansa --rust-target rust/target --output build-migration/M2
python3 tests/parity/run.py fault --suite protocol --cpp-build build-ansa --rust-target rust/target --output build-migration/M2-fault
python3 tests/parity/run.py check --milestone M2 --evidence build-migration/M2
```

Матрицы требуют 7303 compare и 195 fault cases, обе SSE fixtures, реальные
localhost HTTP transfer failures и pure counter executors. Fault запускает
drop/reorder mutants production Rust replay builder. Checker заново проверяет
retained requests/responses, полный continuation и нулевое число executors при
отказе; missing/skipped cases, чужая платформа и подменённые raw files не проходят.
Файлы evidence должны быть обычными, без symlinks/hardlinks и в пределах лимитов.
Эти проверки дополняют native C++/Rust gates и independent review из
[M2 report](../../docs/migration/M2.md).

## M1 risk suite

Собери C++ target `ansa_risk_oracle` и Rust examples/bins, затем выполни compare,
fault и check из [M1 report](../../docs/migration/M1.md#проверки-и-воспроизведение).
Оба capture требуют свежие каталоги; checker по умолчанию ищет fault evidence
в `<compare-directory>-fault`, иначе передай `--fault-evidence PATH`.

Матрицы требуют 602 compare и 286 fault cases, включая localhost H1/H2/TLS,
synthetic proxy и private supervisor. Raw stdin/stdout/stderr, timeout partial
output, identities и observations сохраняются до semantic assertions. Checker
проверяет actual raw outputs повторно; неизвестное расхождение или пропущенный
case не проходят. Три reviewed stricter deviations описаны в report и manifest.
M1 checker не заменяет полный C++ native gate, Cargo checks и independent review.

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

Toolchain checker также запрашивает версию через `cargo rustc --offline --locked --package ansa-linux --lib -- --version` в новом временном target directory. Проверяется компилятор, фактически выбранный Cargo через environment/config и wrappers; `cargo metadata` само по себе этого не доказывает. Проверка не запускает scaffold/runtime и не использует прежние build artifacts.

**`--run-ctest` обязателен для evidence gate.** Без флага capture запускает только `ansa --self-check`, `ansa --offline-demo`, version queries и CTest inventory. Он записывает `ctest.status = NOT_RUN`; `check` отвергает такой bundle как незавершённый. С флагом выполняется полный `ctest --output-on-failure --no-tests=error --output-junit ...`, без фильтра исключений. Неудачный case остаётся в результатах. Harness не собирает и не исправляет C++ автоматически.

Для sanitizer profile:

```sh
cmake -S . -B build-ansa-sanitize -G Ninja -DCMAKE_BUILD_TYPE=Debug -DOMH_SANITIZERS=ON -DOMH_MUTATION_CHILD=ON
cmake --build build-ansa-sanitize -j 2
ASAN_OPTIONS=detect_leaks=1 UBSAN_OPTIONS=halt_on_error=1 python3 tests/parity/run.py capture --cpp-build build-ansa-sanitize --output build-migration/cpp-sanitize --run-ctest
python3 tests/parity/run.py check --milestone M0 --evidence build-migration/cpp-sanitize
```

Manifest перечисляет все **87** зарегистрированных baseline IDs. `OMH_MUTATION_CHILD=ON` исключает только `mutation-drop` и `mutation-reorder`, оставляя **85**; `check` допускает такой набор только для `OMH_SANITIZERS=ON`. Пустая registration, missing/duplicate ID, skipped/disabled case, timeout, ошибка CTest или неисполненный mutant не проходят проверку. Nix Release 87/87 и сохранение executable/Nix closure остаются отдельными gates: сохранить output/log `nix build --no-update-lock-file` вместе с checkpoint. Одного Debug bundle для M0 недостаточно.

Булевы значения cache трактуются как в CMake: `TRUE`, `YES`, `1` и другие истинные значения включают те же sanitizer/mutation gates, что и `ON`. Регистр обычных boolean keywords не влияет; `NOTFOUND` и суффикс `-NOTFOUND` проверяются с учётом регистра. Для любого включённого sanitizer profile обязательны записанные `ASAN_OPTIONS=detect_leaks=1` и `UBSAN_OPTIONS=halt_on_error=1`.

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

`perf_budget.json` содержит согласованный `FROZEN_M0` baseline и численные пороги для safe CLI, footprint и Release target-only `-j2` builds. Raw observations, exact one-shot measurement script, hashes и методика сохранены по ссылке `evidence.directory`; результаты кратко описаны в [M0 handoff](../../docs/migration/M0.md). Первый ошибочный RSS-прогон сохранён как rejected attempt; бюджеты основаны только на исправленном v2.

CLI wall time измеряет launch→exit readiness, RSS снимается отдельными GNU time samples без Python launcher floor. Cold обозначает только страницы самого executable с `mincore=0`, не все зависимости и системные caches. Build RSS содержит maximum individual process high-water и sampled sum по owned descendants через 20 ms, включая отдельные Ninja job groups; sampled peak не является точным physical RAM high-water. Сравнение требует одинаковых hardware/profile/jobs/cache protocol/adapters; три clean и девять incremental builds дают описательные статистики.

Обычный capture `elapsed_seconds` остаётся диагностическим временем, а `perf` в M0 по-прежнему возвращает unimplemented/nonzero. Production first frame, history и прочие performance gates явно отложены до M7/M8 с контролируемым synthetic filesystem/OS environment. Scaffold и safe CLI не измеряют production bootstrap; отложенный metric не считается PASS.

Проверки самого harness:

```sh
python3 -m unittest discover -s tests/parity -p 'test_*.py' -v
git diff --check
```

Новые harness tests не регистрируются как CTest и не увеличивают число baseline cases.
