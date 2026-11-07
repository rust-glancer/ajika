# Your first queries

We will count words in a captured text file. This is small enough to follow in full,
but has the same useful boundary as a larger analyzer: changing whitespace changes
the text without changing the words.

The complete program is
[the word-count example](https://github.com/rust-glancer/ajika/blob/main/examples/word-count/src/main.rs).
From this repository, run it with:

```sh
cargo run -p ajika-word-count
```

## Describe the question

An input is data supplied from outside the query graph. Here its key is a path and
its value is the captured text. A query computes an answer from inputs or other queries.

```rust
# extern crate ajika;
# extern crate anyhow;
{{#rustdoc_include ../../../examples/word-count/src/main.rs:queries}}
```

`File::at(path)` addresses one input; it does not read a file from the filesystem.
The generated `text(db)` getter reads the text that the application supplied.

The bodies return owned values through `Result<T>`. The query macro wraps them in
`Arc<T>`, so callers receive `Result<Arc<Vec<String>>>` from `words` and
`Result<Arc<usize>>` from `word_count`.

The context, `&Db`, connects their reads to the active operation.
The remaining arguments identify the question:
`words("notes.txt")` is one call; `words("other.txt")` is another.

Calling `words` inside `word_count` also tells Ajika what the latter relied on.
You do not register the relationship separately. It follows the calls that actually
run, including calls made conditionally.

## Supply inputs and ask

The long-lived `Database` owns the inputs and reusable work. Its `run` method starts
an **operation**: a scope in which query calls can share computed and loaded values.

```rust
# extern crate ajika;
# extern crate anyhow;
{{#rustdoc_include ../../../examples/word-count/src/main.rs:run}}
```

The unit value `()` means this application has no extra host services. The retention
limits are example budgets, not prescribed settings. For a memory database, entry
count and age apply; it does not serialize values merely to enforce the byte budget.

`update_inputs` publishes a batch before the operation starts. Query code can read
those inputs, but cannot replace them while the operation is running.

The first request executes both functions. Each caller receives an `Arc` so it can
share an immutable answer. Keeping the returned `Arc` keeps that answer alive;
it does not keep a live view of future edits.

## Change the text without changing the answer

First ask the same question again, then widen the gap to four spaces:

```rust
# extern crate ajika;
# extern crate anyhow;
{{#rustdoc_include ../../../examples/word-count/src/main.rs:unchanged}}
```

The second operation with unchanged inputs executes neither function. After the
edit, Ajika must run `words` because it read the changed text. Its new vector still
contains `["hello", "world"]`, so Ajika preserves the answer's version.

`word_count` depended on that answer, not directly on the file text. Its dependency
still matches, so its function does not run. The `executed` statistic counts all
query executions in the latest operation, including children.

Now insert a word:

```rust
# extern crate ajika;
# extern crate anyhow;
{{#rustdoc_include ../../../examples/word-count/src/main.rs:changed}}
```

The words differ, so `word_count` runs as well. If a third query used the count,
its reuse would depend on whether that count changed.

This is the central programming model: choose meaningful intermediate answers,
call queries normally, and give each answer an equality relation that covers
everything consumers can observe. Next, we will look more closely at
[input changes](inputs.md).
