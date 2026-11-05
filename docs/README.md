# Working on this book

## Build and preview

From the repository root:

```sh
cargo install --locked mdbook --version 0.5.4
just book
just book-serve
```

The built book is in `target/book`. `book-serve` rebuilds on edits and serves it
locally. `just docs` builds both the book and the workspace's API documentation.

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
