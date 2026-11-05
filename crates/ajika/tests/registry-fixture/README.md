This separate workspace checks automatic registration across a provider crate and a fresh
process. It links only Ajika's `inventory` collector. It also includes a private, uncalled
query so release dead-code elimination cannot accidentally make the test easier.

Run from the repository root:

```sh
cargo nextest run --manifest-path crates/ajika/tests/registry-fixture/Cargo.toml
cargo nextest run --manifest-path crates/ajika/tests/registry-fixture/Cargo.toml --release
```

The release profile enables fat LTO. CI runs this smoke test on its existing native runners.
