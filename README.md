# oh-my-harness

Независимый C++20 protocol foundation для персонального AI coding agent. Namespace `omh`, executable `omh`. Первый milestone работает offline: строгий Responses admission, чистый tool и stateless native replay.

Новая инженерная сессия начинается с [AGENTS.md](AGENTS.md) и [текущего состояния проекта](docs/PROJECT_STATE.md).

## Сборка и проверка

Зависимости и компилятор зафиксированы через `flake.lock`. Проверенная платформа - NixOS/aarch64, ThinkPad X13s. В flake также объявлен x86_64-linux; он пока не проверен.

```sh
nix develop --no-update-lock-file
cmake -S . -B build -G Ninja -DCMAKE_BUILD_TYPE=Debug
cmake --build build -j 4
ctest --test-dir build --output-on-failure --no-tests=error
./build/omh --self-check
./build/omh --offline-demo
```

Сборка installable Nix package с обязательными тестами:

```sh
nix build --no-update-lock-file
./result/bin/omh --self-check
./result/bin/omh --offline-demo
```

ASan/UBSan, из того же `nix develop`:

```sh
cmake -S . -B build-sanitize -G Ninja -DCMAKE_BUILD_TYPE=Debug \
  -DOMH_SANITIZERS=ON -DOMH_MUTATION_CHILD=ON
cmake --build build-sanitize -j 2
ctest --test-dir build-sanitize --output-on-failure --no-tests=error
```

Обычный CTest включает семь suites, две проверки executable и два mutation-теста. Mutation-тесты меняют копии production replay builder внутри build-каталога и требуют конкретного отказа replay oracle. Ошибка компиляции mutant не считается успешным тестом. Sanitizer-конфигурация проверяет те же семь suites и executable; повторные сборки mutants там отключены.

## Границы реализации

- libcurl отвечает за HTTPS bytes; bounded SSE framer - за события; Responses reducer - за semantic admission.
- Admission требует successful terminal, согласованных IDs/model/items/arguments и завершённого framing. До проверки всего batch executors недоступны.
- Owning JSON сохраняет порядок native items, opaque strings и числовые tokens без округления. Replay использует исходные prefix, context epoch и advertised tool snapshot.
- `ToolRegistry` хранит compiled descriptor/schema/validator/executor/revision. Provider получает только descriptions. В production зарегистрирован только детерминированный `get_test_value` без внешних эффектов.
- Commands/events v1, typed identities, cancellation token и snapshot/resync payloads задают границы будущих frontend/runtime.

CLI предоставляет self-check и синтетический round trip. Его final response задан fixture: это проверка протокола, а не inference или оценка модели. Команд для OAuth и live generation в CLI пока нет.

Сетевой adapter с TLS verification собран, но live HTTP/TLS и OAuth lifecycle этим milestone не подтверждены. Политика MIME по умолчанию строгая; отсутствие MIME можно разрешить только явно для точного `POST https://api.openai.com/v1/responses`, `stream:true`, `store:false`, HTTP 200. Неверный или повторный MIME всегда приводит к отказу.

Поддерживается закрытое подмножество Responses events: lifecycle, function arguments, message text/refusal и reasoning summary. Неизвестные events/items отклоняются. Требуются `response.created`, непрерывные unsigned sequence numbers с нуля и точное совпадение returned model с явно выбранной policy. Для reasoning обязательно непустое `encrypted_content`. Это консервативный adapter, а не полная реализация всех Responses capabilities.

Для нового pure tool достаточно `ToolDefinition` и регистрации; пример с аргументами есть в suite `tools`. Поддерживаемая локальная schema: object/array/string/integer/number/boolean/null, все properties обязательны, `additionalProperties:false`; domain validation задаётся compiled validator. Неизвестные schema keywords отклоняются. При изменении реализации нужно менять `implementation_revision`.

Архитектура и ограничения: [docs/architecture.md](docs/architecture.md). Фактические gates: [docs/implementation-report.md](docs/implementation-report.md).

Request profile следует [официальному SIWC Preview contract](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations) и [models/inference contract](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference), сверенным 2026-11-11.
