# Working on this book

The [book](src/README.md) has three parts:

- **Using Ajika** teaches through a small application.
- **Conceptual reference** helps users make design choices without studying the
  implementation. Chapters stand alone and point out the relevant public entrypoints.
- **Inside Ajika** explains how the implementation provides that behavior.

Keep those purposes separate. A reference chapter can use a small example to
explain a distinction, but should not require completing earlier tutorial steps.

## Build and preview

From the repository root:

```sh
cargo install --locked mdbook --version 0.5.4
just book
just book-serve
```

The built book is in `target/book`. `book-serve` rebuilds on edits and serves it
locally. `just docs` builds both the book and the workspace's API documentation.
No mdBook preprocessor plugins are required.

## Publishing

The [Pages workflow](../.github/workflows/pages.yml) publishes the book on pushes
to `main`, or through **Actions → Publish book → Run workflow** on `main`.
It skips publishing while the repository is private.

After making the repository public, enable **Settings → Pages → Build and
deployment → Source → GitHub Actions**, then run the workflow. The workflow does
not enable Pages itself. The default site address is
[`rust-glancer.github.io/ajika/`](https://rust-glancer.github.io/ajika/).

The workflow uses the Pages base path when building, so navigation on the 404
page also works under the repository's URL. Builds and example tests on pull
requests remain in the normal CI workflow.

## Check the examples

```sh
just test-book
```

This builds Ajika's examples into a dedicated Cargo target directory, then runs
mdBook's Rust snippet tests with that directory's dependencies. Keeping that
build separate avoids mixing it with the independently linked replay fixture.

The longer tutorial snippets come from runnable examples under
`examples/`. Named anchors select visible sections;
`rustdoc_include` keeps the rest of each program available to the test.
Hidden imports and setup make smaller snippets complete without distracting
from the part being taught.

Run an example directly when changing its behavior:

```sh
cargo run -p ajika-word-count
cargo run -p ajika-persistence
cargo run -p ajika-outputs
cargo run -p ajika-editor-session
```

Use `text` fences for diagrams and explicitly schematic data. Rust examples
should compile and, where practical, assert the behavior they explain.
Do not turn a failing example into an ignored snippet to keep a build green.

## Check explanations against the implementation

Source links point to this repository's main branch. When moving code, update
the nearby line anchors and verify that each still names the relevant definition.
A valid URL is insufficient if it lands on an unrelated line.

Changes to the evaluator deserve checking both normal reads and validation-only
replay. Changes to checkpointing deserve checking persistent, session, and
operation lifetimes. Explain a limitation where a reader would encounter it,
instead of leaving it only in an implementation note.

Avoid repeating the API documentation field by field. The conceptual reference
should help a reader decide what to use and predict its behavior; exact signatures
belong in rustdoc. Keep implementation walkthroughs in the internals part.
