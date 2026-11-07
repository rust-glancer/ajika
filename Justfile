rustfmt_toolchain := "nightly-2026-05-28"

# List development commands.
default:
    @just --list

# Run behavior tests and doctests.
test: test-workspace test-docs

[positional-arguments]
test-workspace *args:
    cargo nextest run --workspace --locked "$@"

test-docs:
    cargo test --workspace --doc --locked

# Run both Divan suites; pass --test to check every scenario without sampling.
[positional-arguments]
bench *args:
    cargo bench -p ajika --bench runtime --locked -- "$@"
    cargo bench -p ajika_storage --bench storage --locked -- "$@"

[positional-arguments]
fmt *args:
    cargo +{{rustfmt_toolchain}} fmt --all "$@"

lint: (fmt "--check")
    cargo clippy --workspace --all-targets --locked -- -D warnings
    DYLINT_RUSTFLAGS="-D warnings" cargo dylint --all --workspace -- --all-targets --locked

deny:
    cargo deny --locked check

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

# Keep one dependency build for rustdoc's markdown examples.
test-book:
    # Rustdoc's -L lookup cannot distinguish old dependency hashes restored from cache.
    # Cached artifacts can lack CACHEDIR.TAG, so Cargo refuses to clean this directory itself.
    rm -rf -- target/book-tests
    cargo build -p ajika-word-count -p ajika-persistence -p ajika-outputs -p ajika-editor-session --locked --target-dir target/book-tests
    mdbook test docs -L target/book-tests/debug/deps

build:
    cargo build --workspace --release --locked

pr-ready: test lint deny docs test-book
