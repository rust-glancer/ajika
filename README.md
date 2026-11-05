# Ajika

Ajika is a synchronous incremental query library for applications that want to reuse
computation across restarts without keeping every result in memory.

Declare typed inputs and ordinary Rust query functions. Ajika tracks what each query
reads, recomputes affected results, and preserves dependent results when a recomputation
produces the same value. Query bodies return ordinary values; Ajika wraps them in
`Arc<T>` for callers, whether reused from memory, loaded from storage, or just computed.

An operation owns its working set and may run queries on scoped worker threads. On
success, the disk backend checkpoints new results and releases decoded values. The
application supplies input changes and chooses operation boundaries.

Start with [The Ajika Book](docs/src/README.md) for a tutorial, a conceptual reference
for designing your integration, and a walkthrough of the runtime and storage internals.
Run `just book-serve` to read it locally. The [query guide](crates/ajika/README.md)
also provides a compact API introduction.

## Workspace

| Crate | Responsibility |
| --- | --- |
| [ajika](crates/ajika) | Query evaluation, inputs, invalidation, and operation lifecycle. |
| [ajika_macros](crates/ajika-macros) | Query, input, output, and interned-ID declarations, re-exported by Ajika. |
| [ajika_storage](crates/ajika-storage) | Atomic checkpoints using a redb index and compressed immutable value files. |

Ajika is under active development; APIs and cache formats can change. Storage is a
rebuildable cache, and applications remain responsible for acquiring their inputs.

## Examples

Each example is a workspace package under `examples/`. Run these commands from the
workspace root:

| Example | Demonstrates | Run |
| --- | --- | --- |
| [Word count](examples/word-count/src/main.rs) | Basic queries and selective recomputation. | `cargo run -p ajika-word-count` |
| [Persistence](examples/persistence/src/main.rs) | Reusing query results after reopening a database. | `cargo run -p ajika-persistence` |
| [Outputs](examples/outputs/src/main.rs) | Named outputs with independent invalidation. | `cargo run -p ajika-outputs` |
| [Editor session](examples/editor-session/src/main.rs) | Unsaved changes and promotion on save. | `cargo run -p ajika-editor-session` |
| [Handbook](examples/handbook/README.md) | A Markdown editor with live previews, parallel link checking, and persistent reuse. | `just handbook demo` |

The handbook demo runs a scripted editing session. Use `just handbook serve` to
open the editor.

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md) for tool setup and the test layout.

```sh
just test       # behavior tests, doctests, and release replay tests
just lint       # formatting, Clippy, and shared Dylint checks
just deny       # dependency advisories, licenses, and sources
just docs       # build the book and API documentation
just test-book  # compile and run the book's Rust examples
just pr-ready   # all of the above
```

## License

Licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
