# Rust migration scope

Read the repository AGENTS.md, docs/PROJECT_STATE.md and the current milestone in
docs/RUST_MIGRATION_PLAN.md before changes here. Production remains C++.

## M5 boundary

- M4 is GO. M5 authorizes the synthetic owned runtime, cancellation, summary
  loop and typed frontend. Preserve v1 journals and all existing codec gates.
- One Tokio current-thread owner controls state. Journal, provider, tool and
  auth blocking work have explicit owners, bounded queues and joined threads.
- Full batch admission precedes durable intent; recheck authorization and exact
  work identity before dispatch. Cancel bypasses Submit and stops future work.
- Preserve late known outcomes, reject stale previews/completions, pause on
  unknown effects/storage failure and require explicit Resume after restart.
- Reuse existing dependencies and evidence helpers. One final PR; no real
  providers/credentials/tools, process runner, UI, orchestrate or cutover.

## Historical M4 boundary

- M3 is GO. M4 authorizes synthetic journal/session v1, exact context epochs,
  reader then writer, bidirectional C++/Rust append/reopen and crash recovery.
- Preserve OMHJ1 bytes, all eleven event kinds, IDs, native items and hashes.
  Reuse production saved-response admission. Unsupported state stays read-only.
- One canonical append-only journal; shared flock; durable intent before effects.
  Recovery never executes a tool or automatically resumes a turn. Keep known
  outcomes after storage failure without publishing an undurable assistant.
- Use existing dependencies/tests; no real user sessions, credentials, live,
  runtime owner loop, UI, schema change, fsync batching or cutover. One final PR.

## Historical M3 boundary

- M2 is GO. M3 authorizes auth/OIDC and v1 store compatibility using synthetic
  fixtures, explicit private fixture directories and injected exchanges only.
- Preserve registration/credential paths, identities, pending markers, shared
  process locks, atomic replace/fsync and lease/logout ordering. Test both
  C++ to Rust and Rust to C++ read/write/reopen.
- Verify signature and all claims explicitly. Keep one ring TLS provider;
  oauth2 has default features disabled and supplies narrow PKCE primitives.
- Keep secrets out of Debug, ordinary diagnostics, events and journals.
  Never open existing agents' profiles, real credentials or environment files.
- No live login/refresh/logout, keychain migration, multi-profile, journal,
  owner loop, automatic retries or production cutover. One final PR after
  native differential/fault gates and independent review.

## Historical M2 boundary

- M1 is GO. M2 authorizes full offline protocol/SSE/Responses/replay and pure
  tool-batch admission, with C++ remaining the production default.
- Keep production request/replay builders in the test path. Preserve native
  items, numeric tokens, encrypted strings, exact epochs/tools/account and
  legacy hash bytes; compare ordinary request JSON structurally.
- An admitted response must belong to its request and have verified successful
  transport EOF. Its constructor and the successful-transfer proof are closed.
  Reject the entire response/batch before any counter executor runs.
- PreparedBatch is consume-once and not Clone. Saved-response admission uses
  synthetic SSE through the same production decoder. No second validator.
- Port actual assertions and parameterized loops from all seven C++ protocol
  suites, including dependencies, streaming preview and contracts. Execute
  drop/reorder mutants against production replay builders.
- Implement protocol compare/fault and M2 raw-evidence checking. Missing,
  skipped, all-reject or agreeing-but-wrong results must not pass.
- One final PR after native gates and independent review. JSON optimization
  and its early Release cost-probe are deferred; correctness bounds remain.
- No real credentials/model calls, durable executors, journal, auth, owner loop,
  UI, automatic retry or production cutover in this milestone.

## Historical M1 boundary

- M0 is GO. M1 authorizes only JSON/hash, private process supervisor and local
  HTTP/TLS risk spikes plus their synthetic fixtures/adapters. Production is C++.
- Core remains `forbid(unsafe_code)`. Linux wrappers prefer safe rustix/std APIs;
  any necessary unsafe must have a local safety argument and an adversarial test.
- Do not initialize Tokio/auth before the binary's private supervisor dispatch.
- `compare/fault --suite risk` must execute the real C++/Rust adapters, validate
  every required case and retain failures. Other future suites remain nonzero.
- External crates are limited to the current spike needs; inspect the feature
  graph and keep one ring TLS provider. No complete agent/auth/journal/UI migration.

## Original M0 boundary

- This workspace is a scaffold, not an agent runtime. `ansa-rs` reports that the
  runtime is absent and exits unsuccessfully for every invocation.
- Keep two packages, `ansa` and `ansa-linux`, with only the local path dependency.
  Do not add protocol/auth/storage/process/UI implementations or external crates
  without a task for the corresponding milestone.
- Both crates forbid unsafe in M0. Later Linux wrappers require the root safety
  rules; do not move unsafe into the core crate.
- Commit Cargo.lock. Edition 2024 and rust-version/toolchain 1.98.1 match the
  existing pinned Nix package set; do not add a toolchain overlay.

## Checks from the repository root

Use `nix develop --no-update-lock-file` and run:

```sh
python3 tests/parity/check_toolchain.py
cargo build --manifest-path rust/Cargo.toml --workspace --locked
cargo test --manifest-path rust/Cargo.toml --workspace --locked
cargo fmt --manifest-path rust/Cargo.toml --all --check
cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets --locked -- -D warnings
nix build --no-update-lock-file .#ansa-rust
```

Record the actual compiler/Cargo versions, platform, test counts and failures.
The scaffold test proves only that the binary does not claim runtime behavior.
It does not close native X13s, protocol parity, process or live gates. Keep the
C++ default package and executable available throughout M0.
