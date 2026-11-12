# ADR 0001: полная Rust migration с сохранением C++ baseline

Дата: 2026-11-12. Статус: **предложено к реализации по milestones; production cutover не выполнен**.

## Контекст

Аудит `junknewera/ansa@f6c2b3f08151bfd6eeff7f6ad0f8564a2444d8a9` подтвердил работающую C++20 архитектуру с собственным agent loop, strict Responses admission, native replay, OAuth lifecycle, durable OMHJ1 sessions, Linux supervisor и Carbon CLI. Цель владельца — Rust как язык всего runtime при сохранении hackability, небольших зависимостей и корректности. Новый UI поверх C++ этой цели не достигает.

## Решение

Рекомендация — **MIGRATE TO RUST**, условно на прохождении [плана D1–D7](../RUST_MIGRATION_PLAN.md). В отдельной ветке создаётся `rust/` workspace с двумя production packages (`ansa`, `ansa-linux`), Edition 2024 и compiler/MSRV 1.98.1 из уже pinned nixpkgs. Production C++ и package default сохраняются до доказанного parity.

Один owned runtime control owner, bounded channels, отдельные bounded journal/tool/auth I/O workers и fresh Linux reexec supervisor сохраняют существующие state machines. Начальный network choice — единый strict hyper-family/rustls/ring adapter; M1 проверяет headers/trailers/proxy bounds и может обоснованно выбрать reqwest frame adapter. OAuth2 library используется без своего HTTP stack; OIDC/SIWC policy и rotating-refresh persistence остаются явными.

Первые gates проверяют наиболее рискованные границы: C++ canonical JSON/hash domains, opaque replay, v1 journal/auth/evidence read/write compatibility и supervisor на native X13s. Rust memory safety не заменяет authorization, process supervision и durable execution.

## Последствия и условия пересмотра

Преимущество — более проверяемое владение jobs/results/FDs, consume-once batches и меньше lifetime assumptions при сопровождении. Цена — обучение Rust, стоимость переписывания и необходимость доказать совместимость; ускорение, меньший RSS/бинарник и меньший token usage не обещаются.

Если ранние native/compatibility gates не удаётся закрыть или стоимость сопровождения/компиляции мешает владельцу самостоятельно менять систему, сохраняется C++ и пересматривается конкретное решение. Permanent hybrid не является fallback по умолчанию.

Rollback использует сохранённый C++ binary и наиболее свежий совместимый v1 state. После refresh нельзя восстанавливать старые tokens; после side effects нельзя автоматически продолжать из старого journal. Unknown effects остаются blocked.

Удаление C++ — отдельный M10 после offline/live acceptance, explicit authorization, independent review и доказанного rollback. Этот documentation change не реализует migration, не меняет main, не читает реальные credentials и не разрешает платные/live вызовы.
