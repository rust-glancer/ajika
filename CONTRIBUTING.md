# Working on Ajika

Install Rust through rustup. The repository uses the stable toolchain for builds
and a pinned nightly for rustfmt's import grouping. Custom lints use the
[Dylint library](https://github.com/rust-glancer/rust-glancer-lints);
its revision is pinned in the workspace manifest.

Install the development tools:

```sh
cargo install --locked just --version 1.48.1
cargo install --locked cargo-nextest --version 0.9.132
cargo install --locked cargo-dylint --version 6.3.0
cargo install --locked dylint-link --version 6.0.4
cargo install --locked cargo-deny --version 0.19.4
cargo install --locked mdbook --version 0.5.4
rustup toolchain install "$(just --evaluate rustfmt_toolchain)" --profile minimal --component rustfmt
```

Dylint installs the compiler components required by the pinned lint library on its
first run. If a filesystem sandbox prevents it from reading build metadata, run
`just lint` outside that sandbox.

## Tests and checks

`just test` runs three complementary checks:

- Workspace tests exercise query behavior, macro declarations, storage, and maintenance.
- Doctests compile and run the examples in the query guide.
- The separate [replay fixture](crates/ajika/tests/registry-fixture/README.md) links
  a provider and consumer, writes a database, and validates it in fresh processes.
  Its release profile uses fat LTO to exercise automatic registration after linking.

Use `just test-workspace -p ajika` or `just test-workspace -p ajika_storage` for
focused runs. `just test-replay` runs only the cross-process fixture.

The replay fixture has its own workspace and lockfile so its dependency graph and
linking are checked independently. Formatting, Clippy, and cargo-deny cover it too.
Both lockfiles are committed; normal checks use `--locked`.

`just pr-ready` also checks formatting, Clippy, Dylint, dependencies, rustdoc,
and the book's Rust examples. See [Working on this book](docs/README.md)
for its preview and example workflow.
CI runs tests on Linux, macOS, and Windows. Its `pr-checks` job aggregates lints,
tests, and documentation; cargo-deny reports separately so a newly published
advisory does not block unrelated changes.

## Finding the implementation

- [Runtime](crates/ajika/src/runtime/mod.rs): operations, evaluation, and cached validity.
- [Inputs](crates/ajika/src/input/mod.rs): typed fields and atomic input updates.
- [Macros](crates/ajika-macros/src/lib.rs): declaration parsing and code generation.
- [Storage guide](crates/ajika-storage/README.md): checkpoints, compression, and reclamation.

Prefer small behavior tests with readable setup. Keep macros focused on declarations;
evaluation and storage rules belong in the runtime and storage crates.
