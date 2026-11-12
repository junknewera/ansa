# Rust migration scope

Read the repository AGENTS.md, docs/PROJECT_STATE.md and the current milestone in
docs/RUST_MIGRATION_PLAN.md before changes here. Production remains C++.

## M0 boundary

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
