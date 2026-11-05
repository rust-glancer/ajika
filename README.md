# Ajika

Ajika is a persistent incremental query library.

It lets you define _queries_, which look like (almost) normal Rust functions but remember
their outputs, which can be _incrementally_ updated, meaning that if inputs change, only
the relevant queries will be recomputed, and it allows you to _persist_ these computations
to the filesystem, loading them on demand and offloading after they're not longer needed.

Start with [The Ajika Book](https://rust-glancer.github.io/ajika/) for a tutorial, a conceptual reference
for designing your integration, and a walkthrough of the runtime and storage internals.
Run `just book-serve` to read it locally. The [query guide](crates/ajika/README.md)
also provides a compact API introduction.

## ELI5

Imagine that you have a computation that you can define as `analyze(transmute(compute(input))`.
Each of these steps is expensive and takes gigabytes of RAM.

With Ajika, you will turn `analyze`, `transmute`, and `compute` into _queries_. On the first run,
all of them will run, the results will be remembered, and put to the filesystem. While queries
are executing, gigabytes of RAM are taken. Once it finishes, the result is recorded and interim
calculations are removed from memory.

When you call `analyze` again with the same input, nothing runs! Inputs did not change, so we
will just load and return the already computed answer.

Now imagine that you provided `input2`. The input is different, but `compute` happens to return
the same answer. Then `transmute` will not run, `analyze` won't run as well: we can still reuse
the answer we have.

In real world, you will typically have more than 3 queries, and change in inputs will change
only some of the queries. Imagine that you have a text editor with 1000 files. Typically, only
one file would change at a time, leaving 999 unchanged. Remembering computations can make
any kind of work with such a set much more straightforward without having to worry about
hand-written caches in every possible place.

## Why not Salsa

[Salsa](https://github.com/salsa-rs/salsa) is an obvious inspiration, and if your project has no memory
constraints and it works for you, you _absolutely_ should use it.

Ajika works very well for its purposes, and in many cases the overhead it incur won't be _significant_,
but it's ultimately slower by design because it's build for persistence, not for the memory.

Ajika shines when you have expensive and reusable computations, and you don't want these to be in RAM
when they are not used. This could be a good use case for, say, an LSP server, where user might be
editing files only a fraction of the day, while keeping an IDE open all the time.

The API of Ajika is _similar_ to Salsa, but I have changed many things to my taste. It shouldn't feel
alien, however, if you're familiar with Salsa.

## LLM usage disclaimer

LLMs are used to work on this project. LLMs are not used as a brain replacement.

I've invested many hours into this project (actively working on it, not waiting for the prompt to complete),
so I hope you won't consider it's slop. But if it's slop, then, welp, it's _my_ slop, not the clanker's :)

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

## Acknowledgements

This library is heavily inspired by [salsa](https://github.com/salsa-rs/salsa), a (mainly) in-memory
incremental computation library.

The design also took some inspiration from [turbo-tasks from next.js](https://github.com/vercel/next.js/tree/canary/turbopack/crates/turbo-tasks) and [DICE from buck2](https://github.com/meta/buck2/tree/main/dice). [picante](https://github.com/bearcove/dodeca/tree/main/libs/picante) is a honorable mention.

## License

Licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
