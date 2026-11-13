# Состояние ansa

## Rust migration: M3 закрыт (GO)

**Rust last verified milestone - M3 GO, 2026-11-13**, code checkpoint
[`0ba175f`](https://github.com/junknewera/ansa/commit/0ba175f15cc6dbbd4d842c0fc52981cd88cc07b5),
tag `migration/m3-auth-v1`. Реализованы synthetic RS256/OIDC, PKCE/callback,
registration/login/refresh/logout, совместимый private store v1, cancel/leases
и lazy dispatch guard. Production/default остаётся C++20.

X13s/NixOS/aarch64: C++ Debug и Nix Release **87/87**, ASan/UBSan **85/85**;
clean auth compare **220/220**, fault **273/273**, sanitized compare **220/220**,
raw checker PASS. Cargo и Nix Rust **35/35** integration functions + **7/7**
compile-fail doctests; harness **51/51**, Linux preflight **11/11**, toolchain,
fmt/clippy default+features PASS. M1 regressions **602/286**, M2 **7303/195**,
оба Rust replay mutants пойманы. 62 native C++ inputs совпали с Nix source.

Два независимых reviews закрыты. В обе стороны проверены read/write/reopen,
один concurrent refresh, 21 crash boundary с recovery другим binary,
29 file/store guards и отсутствие повторного ambiguous POST. Пять stricter
RSA/path corrections согласованы явно. C++ callback, FIFO и NUL path guards
усилены и прошли полную native matrix. Dependencies: 146 external (+67), прежние
versions/checksums сохранены, второго HTTP/TLS client нет.

Evidence, logs, binaries, Git bundle, reviews и GC-rooted Release outputs -
`~/Workspace/pet/ansa-baselines/m3-0ba175f/`. Подробности и команды -
[M3 report](migration/M3.md). Следующая отдельная задача - M4: journal/session
v1 и recovery; автоматически не начинать. Live/cutover/performance parity
остаются вне этого gate; JSON optimization отложена.

## Rust migration: M2 закрыт (GO)

**Rust last verified milestone - M2 GO, 2026-11-13**, code checkpoint
[`b9f254c`](https://github.com/junknewera/ansa/commit/b9f254c77dd597c6be03ed39aae0234d74f889d3),
tag `migration/m2-protocol-replay`. Реализованы bounded SSE/Responses,
lossless native replay, закрытые admission/result types, immutable tool snapshot,
полный batch preflight и consume-once execution с pure synthetic counters.
Production/default остаётся C++20.

X13s/NixOS/aarch64: C++ Debug и Nix Release **87/87**, ASan/UBSan **85/85**;
clean protocol compare **7303/7303**, fault **195/195**, оба Rust production
replay mutants пойманы требуемыми assertions. Sanitized C++ compare
**7303/7303**, raw checker PASS. M1 regressions: compare **602/602**, fault
**286/286**. Cargo **19/19** integration functions и **7/7** compile-fail doctests,
harness **43/43**, preflight **11/11**, fmt/clippy/default+feature и Nix Rust PASS.
Cargo.lock и 79 external dependencies не изменены, TLS только rustls/ring.

Независимые protocol/admission и transport/harness reviews закрыты. Исправлены
нарушения argument lifecycle и полноты terminal, ранние preview/limit guards,
binding batch results и evidence checker. C++ Json accessors сохраняют custom
limits; эта узкая production-правка прошла всю native matrix, старый M1 fallback
сохранён. 62 C++ native inputs в Nix Release source совпадают с checkpoint.

Evidence, binaries, failed attempts, logs, source proof и GC-rooted Release
outputs - `~/Workspace/pet/ansa-baselines/m2-b9f254c/`. Scope, review и точные
команды - в [M2 report](migration/M2.md). Следующая отдельная задача - M3
synthetic auth/OIDC/store; автоматически не начинать. Live, cutover и performance
parity этим этапом не закрываются; JSON optimization отложена по решению пользователя.

## Rust migration: M1 закрыт (GO)

**Rust last verified milestone - M1 GO, 2026-11-13**, code checkpoint [5324d2a](https://github.com/junknewera/ansa/commit/5324d2ad8235f636f0b5865d9384c0ba101d1208), tag `migration/m1-risk-spikes`. Реализованы synthetic JSON/hash, private early-exec supervisor и localhost HTTP/TLS/proxy spikes. На X13s/NixOS/aarch64: Debug и Nix Release CTest **87/87**, ASan/UBSan **85/85**, sanitized C++ oracle compare **602/602**, clean compare **602/602**, fault **286/286** и raw replay checker PASS. Cargo **4/4** integration functions включают 11 process и 7 H2 cases; harness **20/20**, Linux preflight **11/11**, fmt/clippy и Nix Rust package прошли.

Independent JSON/process/transport review закрыт без открытых findings; три stricter transport corrections зафиксированы явно. Evidence, binaries, logs и GC-rooted Release closures сохранены в `~/Workspace/pet/ansa-baselines/m1-5324d2a/`. C++ executable SHA-256 совпадает с M0 baseline, production/default остаются C++20, budgets сохранены. Scope, ограничения и команды - в [M1 report](migration/M1.md). Следующий шаг - M2: полный protocol, Responses/replay и batch admission; M1 не является Rust runtime или performance parity.

## Rust migration: M0 закрыт (GO)

M0 toolchain/baseline закрыт 2026-11-13: проверенный code/budget checkpoint [6fb1073](https://github.com/junknewera/ansa/commit/6fb10735a9832f1570ed7cb812c1c1f3f4ebdb89), tag `migration/m0-toolchain-harness`. Исходная реализация - `e35b765`, review fixes - `a905212`. Scaffold не является Rust runtime. C++ last known good остаётся отдельным baseline ниже.

Финальная native проверка `6fb1073` на `x13s`, NixOS 26.11/aarch64, kernel 7.2.9, pinned GCC 16.2.0: Debug CTest **87/87**, Nix Release **87/87**, ASan/UBSan **85/85** с `detect_leaks=1`/`halt_on_error=1`; оба mutants прошли в Debug/Release. Linux preflight **11/11**, включая procfs children. Новые Debug/sanitizer capture/check прошли из clean checkpoint с frozen budget; sanitizer использовал `TRUE/YES`. Harness/toolchain regressions **12/12**, Rust scaffold **1/1**, build/fmt/clippy/Nix Rust package и PTY backpressure прошли. Фактические rustc 1.98.1, Cargo CLI 1.98.0. Исторические x86_64 и промежуточные native результаты сохранены в [M0 closeout](migration/M0.md).

Native resource baseline снят на snapshot `7b84446`: safe CLI warm/cold-executable wall/RSS, binary/strip-all/Nix closure sizes, три clean и девять incremental Release `-j2` builds. Clean максимум **39,61 s / 743,23 MiB sampled aggregate RSS**, incremental **3,31 s / 311,61 MiB**. Пользователь явно принял численные пороги в [perf_budget.json](../tests/parity/perf_budget.json); **17/17** baseline budget checks прошли. Финальный Nix executable имеет тот же SHA-256, что измеренный baseline. Это не Rust runtime performance PASS.

Оба P2 закрыты. Независимый review проверил исправления, v2 raw evidence, методику и конкретные budgets; открытых подтверждённых findings нет, `git diff --check` чистый. Raw observations/script/hashes, финальные captures, build manifest и C++ fallback с indirect Nix GC root сохранены в `/home/gusevsaint/Workspace/pet/ansa-baselines/m0-resource-7b84446-v2/`; прежний fallback `m0-a905212` также сохранён. Первая ошибочная RSS-попытка помечена rejected и в budgets не входит.

Следующая отдельная задача - **M1: JSON/hash compatibility, Linux supervisor и HTTP frame/trailer spikes** по плану. Автоматически M1 не начинать. Production first-frame, history и остальные performance gates явно относятся к M7/M8; live и cutover не разрешены M0 GO.

## C++ last known good и история acceptance

Обновлено 2026-11-12. **Carbon UI и быстрый live-старт проверены**. Last known good для кода - [28eb41f](https://github.com/junknewera/oh-my-harness/commit/28eb41f0af449e943811cfbf9a4053f2dd7983ee) на `main`. Пройдены Debug, Release Nix, ASan/UBSan, независимый review, визуальная проверка фактического PTY и live dispatch. Предыдущие baselines - история/streaming/basic-plan `93393ab`, локальный агент `66250f7`, SIWC `e2f8e1c`, persistent offline sessions `c0295bd`, offline runtime `10a11dd` и protocol `ede828d`. [Новая проверка](implementation-report.md#carbon-ui), [предыдущий milestone](implementation-report.md#carbon-cli).

## Carbon UI и запуск

Пользовательская приёмка выявила недостаток `93393ab`: функциональные тесты не подтверждали соответствие оформления выбранному Carbon. Этот разрыв закрыт отдельной визуальной проверкой фактического production frontend в PTY.

- Компактная высота под содержимое, word wrapping, безопасное базовое оформление Markdown для assistant, буквальный пользовательский текст. Заголовок с cwd/model, мятные акценты, широкое поле ввода и читаемый footer. Структурированные роли не извлекаются из содержимого сообщения; preview и transcript используют один renderer.
- Проверены PNG кадров 100×32 и 80×24 на пользовательском примере CPU, а также сокращение области после длинной истории. Старые строки очищаются; лишняя пустота перед вводом убрана. Подтверждённый финальный preview сохраняется до transcript page без мерцания; tool-bearing commit не отображается как завершённый turn.
- Live-старт больше не вызывает access/refresh и каталог моделей до первого кадра: binding берётся из локального статуса. `AgentAuthorization` выполняет сетевые проверки в dispatch worker после Submit, сохраняет account/session/model policy, logout и явный Resume.
- PTY-замеры отдельного бинарника: прежний live **3,608/3,136 с**, новый **0,020/0,017 с** до кадра; offline **0,015 с**. Генерации и tools в этих замерах не было. Это замер открытия интерфейса; первый ответ продолжает зависеть от сети. Задержка запуска не была свидетельством memory leak.

## Проверка `28eb41f`

`x13s`, NixOS/aarch64, GCC 16.2.0: Debug **87/87**, Nix Release **87/87**, ASan/UBSan **85/85** с detect_leaks=1 и halt_on_error=1. UI и auth-lifecycle прошли по 20 повторов, **40/40**. Установленный executable проходит self-check и offline roundtrip.

Независимый Terra low review завершён без открытых подтверждённых замечаний. Исправления покрывают multiline fences в preview, literal `assistant manager`, перенос bold, NO_COLOR, динамическую высоту и auth dispatch/session boundaries. Реальный первый запрос после быстрого открытия вернул STARTUP_OK; проверка каталога и авторизации выполнялась на пути dispatch. `git diff --check` чистый. Следующий согласованный спринт остаётся orchestrate.

## Carbon CLI

- Диалог в основном буфере терминала, широкая строка ввода и компактный статус. UTF-8 не зависит от установленной локали, управляющие символы не исполняются терминалом. Pipe, one-shot и legacy JSONL сохраняют прежний путь вывода.
- Первая страница истории появляется до первого prompt: три последних обмена и незавершённый запрос. Ctrl+F раскрывает/скрывает диалог; PgUp/PgDn листают и загружают bounded страницы. Ввод сохраняется при просмотре истории и отказе занятого runtime.
- Transcript строится из canonical accepted/done records независимо от model context и compact. Неизменяемые блоки ограничивают стоимость обновления конца истории. Фронтенд получает страницы с query echo и восстанавливается после resync. Ошибка fsync не продвигает публичный transcript до несохранённого assistant.
- Live HTTP chunks дают предварительный текст до terminal. Preview имеет generation identity, состояния preview/committed/discarded и snapshot/resync. Cancel, ошибка transport/admission/batch и storage failure не превращают фрагмент в успешный ответ. Tools по-прежнему требуют полной проверки и durable intent.
- Tab/Shift+Tab переключают basic/plan между задачами, slash-команды служат альтернативой. Режим записан в turn, входит в exact request provenance и сохраняется при Resume. Общая история продолжается в обоих режимах. Plan использует backend snapshot только из tools с read_only, без произвольного shell.
- Следующий согласованный спринт - orchestrate: дочерние агенты, их задачи и состояния, сбор и проверка результатов. После реализации он станет третьим режимом Tab. [Границы](TODO.md).

## Проверка `93393ab`

Native `x13s`, NixOS/aarch64, GCC 16.2.0: Debug CTest **87/87**, Release Nix package **87/87**, ASan/UBSan **85/85** с detect_leaks=1 и halt_on_error=1, без сборок mutants. Шесть новых frontend/PTY suites прошли по 20 повторов, **120/120**. Установленный `result/bin/ansa` - ELF64 AArch64, self-check и offline roundtrip успешны. Отдельная Python PTY backpressure regression прошла в pinned Nix shell.

Независимые runtime/provider/session и CLI/TTY review завершены без открытых подтверждённых findings. Закреплены регрессии fsync failure при Done, invalid/mixed tool batch с нулём executors, поздних preview после Cancel, outbox overflow, mode replay/Resume, byte-limited forward/backward paging, sticky input overflow, busy input preservation, первой истории до prompt, locale C, UTF-8/emoji/combining, resize и восстановления termios после SIGTERM. Работу выполняли native агенты GPT-6.1 Terra с low reasoning по запросу пользователя.

Live Terra: plan выполнил list_directory/read_file, вернул PLAN_OK и marker без изменения файлов; basic после transport failure и явного Resume выполнил run_command, создал и прочитал файл, вернул BASIC_OK. Содержимое совпало с marker. Ctrl+F/PgUp/PgDn сохранили черновик. Повторные `--session` и `--continue` восстановили обе реплики и basic; финальный `--continue` показал историю до первого prompt. Credentials и synthetic journal остались вне репозитория. `git diff --check` чистый.

## Переименование и реализованный milestone

Выбрано имя **ansa**. Рабочее дерево перенесено в `/home/gusevsaint/Workspace/pet/ansa`. Прежний путь `oh-my-harness` пока является ссылкой на него для текущего Codex turn и старых build paths. Cwd текущей сессии Codex обновлён штатным app-server API; `thread/read` и фильтр `thread/list`, используемый обычным resume picker, подтверждают новый путь. Rollout и внутренние БД вручную не редактировались. После переноса прошли reducer, self-check и offline roundtrip: **3/3**, Git diff чистый.

Универсальный локальный многошаговый агент реализован в согласованном scope:

- Каталог запуска служит рабочим контекстом; Git не требуется. Агент может исследовать файлы и окружение за пределами этого каталога в рамках прав пользователя.
- Обычный текстовый CLI и executable `ansa`. Новый запуск создаёт новую сессию; `--continue` выбирает последнюю сессию этого каталога, `--session` - указанную. Существующий typed JSONL сохраняется.
- Несколько раундов model → tools → continuation на turn. Обычный ненулевой exit команды возвращается модели как известный результат, чтобы она могла выбрать следующий шаг. Ошибка provider, неизвестный эффект и ошибка storage сохраняют fail-closed поведение.
- Локальные tools для просмотра каталогов, чтения и поиска файлов, управляемого запуска процессов. ProcessRunner ограничивает время и вывод, прекращает свою группу процессов по Cancel и освобождает ресурсы. Нет автоматического повтора неизвестного эффекта после crash.
- Full access по умолчанию для обычных действий. Sudo и повышение привилегий по умолчанию запрещены; это правило обеспечивает backend, а не обещание модели. Ask, интерактивный sudo и hooks в текущий этап не входят. Встроенного правила TRASH нет.
- Структурированные пользовательские настройки в `~/.ansa/config.json`, проектные - в `<cwd>/.ansa/config.json`. Их разбор, проверка и приоритеты детерминированы. Проектные настройки поведения не могут снять пользовательский запрет повышения прав. `AGENTS.md` остаётся отдельным источником текстовых инструкций с учётом родительских и вложенных каталогов.
- Лимиты раундов, процессов и вывода задаются явно; достижение лимита не приводит к бесконтрольному продолжению или повтору уже выполненного эффекта. Сохраняются bounded queues, один control owner, targeted Cancel, IDs, snapshot/resync, canonical journal и exact replay provenance.
- Переименование текущих project/CLI/package labels выполняется с сохранением существующего входа и проверкой совместимости сохранённых данных. Приёмка включает native build, полный CTest, ASan/UBSan, adversarial process/recovery tests, независимый review и короткий live сценарий в каталоге без Git.

Реализация - `66250f7`. [Отложенные возможности](TODO.md).

## Что работает

| Компонент | Поведение |
|---|---|
| Native runtime | C++20, pinned Nix flake, ELF64 AArch64 на X13s. GCC 16.2.0, libcurl 8.22.0, yyjson 0.12.0, OpenSSL 3.5.8. Один control owner, bounded mailboxes, отдельные generation/tools, storage и stop workers. |
| Protocol и tools | Строгие SSE/JSON/HTTP/Responses admission и batch validation. Обычный CLI регистрирует list/read/search файлов, run_command и get_test_value; legacy JSONL сохраняет pure snapshot. Оба replay mutants обнаруживаются. |
| Многошаговый loop | Default CLI 32 раунда и 64 calls на сегмент Resume; hard cap 64/256 на turn. Уникальные operation IDs, полный native continuation, known failed outcomes и явное продолжение после budget pause. |
| ProcessRunner | Noninteractive supervisor/subreaper, bounded time/output, Cancel/timeout/runtime crash cleanup. Durable consumed marker и outcome evidence предотвращают повторный запуск; неизвестный эффект блокирует Resume. Exec elevation отключён, full access не является sandbox. |
| Config и инструкции | Пользовательский и проектный JSON, типы/лимиты/приоритеты проверяет backend. Иерархия AGENTS и scoped instruction tokens, protected roots и nofollow/single-link file reads. |
| Canonical journal | Exclusive session lock, append-only framed records и hash chains. CommandAccepted, intent, outcome и Done проходят fsync barriers. Синхронизируются также parent/session directories. |
| Recovery | Явный Resume после restart. Исполненные calls не повторяются. Для process intent startup проверяет artifact supervisor и синхронизирует canonical outcome. Без достоверного результата effect_unknown блокирует Resume. |
| History и compact | Новый turn получает контекст завершённых turns. Live prompt compact сохраняет сводку старой истории, два последних завершённых turn и pending user без изменений. Summary provenance и exact epoch синхронизируются до generation. Прежний offline compact journal читается. Полная история остаётся в журнале. |
| Ошибки хранения | Snapshot и Cancel доступны при блокированном storage worker. При write/sync failure dispatch прекращается, известный outcome остаётся в публичном snapshot с durable:false. Оборванный хвост сохраняется, новые записи идут в связанную part. Повреждение середины блокирует запись. |
| Frontend v1 | Submit с per-turn mode, Cancel, GetSnapshot, GetTranscript, Resume, typed IDs и incarnation. Durable Submit dedup проверяет также mode. Bounded transcript pages и preview со snapshot/resync. |
| Own SIWC | Собственная регистрация, браузерный PKCE/state/nonce, RS256/JWKS, account/workspace binding, один профиль, приватные файлы 0600, rotating/concurrent refresh и logout. Неопределённый обмен не повторяется, полный remote revoke не подтверждается без оснований. |
| Live runtime | Account catalog, явные requested/returned model, store:false/stream:true, общий admission и replay. Auth проверяется после durable intent; lease удерживается до возврата executor. Logout stop передаётся активному tool. Отказ сохраняет not_started, продолжение - через Resume. |
| CLI | Carbon в основном буфере терминала, streaming, три последних обмена, Ctrl+F и PgUp/PgDn. Tab/Shift+Tab и /basic /plan, --continue/--session/--cwd, /status /cancel /resume /exit. Неблокирующий control, RAII termios, JSONL/auth/self-check сохранены. |

Черновик `dc6766f` в `wip/offline-runtime` сохранён. Проверенные commits и последующие изменения доступны в Git.

## Проверка `66250f7`

Native NixOS/aarch64 на `x13s`, GCC 16.2.0: Debug CTest **81/81**, Release Nix package **81/81**, ASan/UBSan **79/79** с leak detection и halt_on_error. Десять новых loop/process/auth/CLI suites прошли по 20 повторов, **200/200**. Установленный `result/bin/ansa` - ELF64 AArch64; self-check, offline demo и сохранённый auth status успешны. Production CLI проверен на UTF-8, new/continue/explicit session, status/exit и configuration mismatch. `git diff --check` чистый.

Независимые runtime и security review завершены; все findings исправлены и закреплены regression tests. Проверены early Cancel при blocked accepted fsync с нулём generation dispatch, resync/full stdout/SIGINT, tool recovery после crash с known artifact и failed outcome append, duplicate workers, signal inheritance, evidence identity и file/instruction aliases. [Подробности](implementation-report.md#локальный-многошаговый-агент-ansa).

В live каталоге без Git пройдены пять вызовов `list_directory → read_file → run_command(exit 7) → run_command(exit 0) → read_file`, затем Done. Итоговый файл, process evidence и canonical journal проверены. Повторный запуск `--continue` использовал прежний вход и ту же сессию, получил marker предыдущего turn без tools и сохранил второй Done. Credentials и synthetic live journal находятся вне репозитория.

## Проверка `e2f8e1c`

NixOS/aarch64, GCC 16.2.0: Debug CTest **56/56**, Release Nix package **56/56**, ASan/UBSan **54/54** с detect_leaks=1 и halt_on_error=1, без сборок mutants. Семь новых concurrency suites прошли по 20 повторов, **140/140**. Production executable проходит self-check и offline demo; PTY backpressure regression прошла. `git diff --check` чистый. Независимые runtime и security review состоялись, все findings исправлены и закреплены регрессиями.

Собственный браузерный вход и каталог моделей проверены вживую. Шесть synthetic turns завершены, включая два прохода model → get_test_value → continuation → Done. Между запусками сохраняются вход, история и durable Submit dedup. Prompt compact применился трижды; marker старой истории сохранился после compact и restart. Пятый запрос после ошибки транспорта восстановлен явным Resume и дошёл до Done.

Реальный refresh прошёл около 14:00 МСК после разрешённого сервером времени: новые credentials сохранены, expires_in снова 3600, каталог из восьми моделей доступен. Шестой live turn с обновлённым входом завершился ответом REFRESH_OK, без повторной авторизации. Тесты также проверяют rotating refresh между двумя процессами, crash pending, scope loss, logout во время refresh/login, запрет dispatch после logout, удержание lease на executor и отсутствие ложного подтверждения отзыва старых grants. Реальные credentials и live journal находятся вне репозитория.

## Проверка baseline `c0295bd`

На X13s, NixOS/aarch64, Debug CTest **37/37**, включая оба replay mutants. Release Nix package **37/37**. ASan/UBSan с leak detection и остановкой на UB **35/35** без сборок mutants. Пять новых concurrency suites прошли по 20 повторов **100/100**. PTY regression с заполненным stdout прошёл. Установленный executable прошёл self-check, offline demo и persistent JSONL handshake.

Crash tests посылают SIGKILL собственным дочерним процессам после fsync acceptance, admitted response, intent, outcome, continuation и Done. Дополнительно проверены crash после compact и между двумя tools batch. Generation и executor не обходят durability barriers. Дубликаты Submit/Resume не создают повторное выполнение.

Независимый adversarial review проверил journal/replay, control owner, checkpoints, cancellation, shutdown и immutable state ownership. Все findings исправлены и закреплены регрессиями. В их числе known outcome при failed outcome/cancel append, fsync parent directory, compact item reserve, Resume replay, part-count limit и отсутствие ложных tool events до dispatch. `strace` подтвердил порядок fsync parent, part и session directory. `git diff --check` прошёл.

Команды воспроизведения - в [README](../README.md). Ранее `10a11dd` проходил CTest **20/20**, ASan/UBSan **18/18**. Protocol baseline `ede828d` проходил **11/11** и **9/9** соответственно.

## Ограничения и следующий шаг

- Production CLI использует live SIWC и локальные tools; offline режим остаётся детерминированным mock/get_test_value. Prompt compact проверен на коротком marker-сценарии; качество длинных сводок и token-aware budget остаются открытыми. Native compact endpoint не нужен live пути. HTTP body ограничен, production streaming проходит через общий reducer. Preview не заменяет полный admission; после restart восстанавливается подтверждённый transcript. [Контракт и границы](architecture.md#compact).
- Credentials хранятся plaintext с правами 0600. Несколько профилей и системный secret store отложены. Identity-only scope поддержан при полном token response; ответ только с ID token без access/refresh отклоняется. Remote revoke проверен fixtures; отдельной реальной logout-приёмки пока нет. Неопределённый либо забытый прежний grant оставляет remote_revocation_confirmed:false.
- SIGKILL и fault injection проверены. Физическое отключение питания, поведение накопителя при power loss, ThreadSanitizer, x86_64-linux и длительные сессии пока открыты. Durability опирается на успешный fsync файловой системы и оборудования.
- Session journal version 1 восстанавливается полным replay. Миграции, отдельные индексы и ротация по общему размеру отложены. По умолчанию лимит Submit IDs - 4096, records - 32 MiB, parts - 4096. Compact уменьшает активный контекст; история журнала сохраняется.
- Full access не является sandbox. no_new_privs запрещает повышение через exec, но не обращение к уже разрешённым privileged brokers. При SIGKILL самого supervisor результат unknown; без cgroup не гарантируется прекращение всех отделившихся потомков. Нет автоматического повтора неизвестного эффекта.
- Reconciliation неизвестных эффектов и отдельный compiled file replacement tool требуют самостоятельного контракта. Произвольный shell не обещает preimage/conflict semantics или atomic CAS с внешними writers. Advisory lock и проверки inode/size имеют те же границы.
- Изменение effective config или начальных AGENTS требует новой сессии из-за exact context epoch; basic/plan задаются отдельно на turn. Полная модельная история по-прежнему восстанавливается replay, стоимость копирования submit map и длительные сессии требуют отдельной проверки. Следующий спринт - orchestrate; MCP, hooks, Ask, интерактивный sudo, отдельный TUI и token-aware budget остаются в [TODO](TODO.md).

## Первоисточники

Начинай с `src/`, `include/omh/`, `tests/`, [README](../README.md), [architecture.md](architecture.md) и [implementation-report.md](implementation-report.md). Внешние справочные материалы на X13s читай для конкретного вопроса.

| Источник в `/home/gusevsaint/harness-research/` | Что искать |
|---|---|
| `20261110T194330Z/{00-executive-summary,05-auth-and-api-feasibility,06-personalized-requirements}.md` | Исходная постановка и границы SIWC. |
| `20261110T194330Z/{07-system-design,08-architecture-decisions,09-implementation-roadmap,10-evaluation-plan,sources}.md` | Дизайн, решения, очередность, оценка и ссылки на первоисточники. |
| `20261110T202238Z-siwc-probe/result.md` | Результат предыдущей live проверки SIWC. |
| `20261110T203326Z-p0-function/{p0-report.md,protocol_probe.py,test_protocol.py,review/review.md,review/closure.md}` | Function-call P0, исходные adversarial cases и замечания независимого review. |

Используй reports, source и synthetic fixtures. Credential stores, токены и transcripts не входят в handoff.
