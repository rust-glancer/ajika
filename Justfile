rustfmt_toolchain := "nightly-2026-05-28"
replay_manifest := "crates/ajika/tests/registry-fixture/Cargo.toml"

# List development commands.
default:
    @just --list

# Run behavior tests, public examples, and cross-process registration checks.
test: test-workspace test-docs test-replay

[positional-arguments]
test-workspace *args:
    cargo nextest run --workspace --locked "$@"

test-docs:
    cargo test --workspace --doc --locked

# The separate workspace links a consumer with fat LTO and reopens its database.
[positional-arguments]
test-replay *args:
    cargo nextest run --manifest-path {{replay_manifest}} --target-dir target --release --locked "$@"

# Run both Divan suites; pass --test to check every scenario without sampling.
[positional-arguments]
bench *args:
    cargo bench -p ajika --bench runtime --locked -- "$@"
    cargo bench -p ajika_storage --bench storage --locked -- "$@"

[positional-arguments]
fmt *args:
    cargo +{{rustfmt_toolchain}} fmt --all "$@"
    cargo +{{rustfmt_toolchain}} fmt --manifest-path {{replay_manifest}} --all "$@"

lint: (fmt "--check")
    cargo clippy --workspace --all-targets --locked -- -D warnings
    cargo clippy --manifest-path {{replay_manifest}} --target-dir target --workspace --all-targets --locked -- -D warnings
    DYLINT_RUSTFLAGS="-D warnings" cargo dylint --all --workspace -- --all-targets --locked

# Share the root policy with the independently resolved replay fixture.
deny:
    cargo deny --locked check
    cargo deny --manifest-path {{replay_manifest}} --locked check --config deny.toml

# Build the teaching book and API reference.
docs: book docs-api

docs-api:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked

book:
    mdbook build docs

book-serve:
    mdbook serve docs

# Run the interactive handbook, export pages, or follow its editing walkthrough.
[positional-arguments]
handbook *args:
    cargo run -p ajika-handbook --locked -- "$@"

# Keep one dependency build for rustdoc's markdown examples, separate from replay.
test-book:
    # Rustdoc's -L lookup cannot distinguish old dependency hashes restored from cache.
    # Cached artifacts can lack CACHEDIR.TAG, so Cargo refuses to clean this directory itself.
    rm -rf -- target/book-tests
    cargo build -p ajika-word-count -p ajika-persistence -p ajika-outputs -p ajika-editor-session --locked --target-dir target/book-tests
    mdbook test docs -L target/book-tests/debug/deps

build:
    cargo build --workspace --release --locked

pr-ready: test lint deny docs test-book
