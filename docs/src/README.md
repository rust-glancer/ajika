# About Ajika

An editor often asks the same questions after each edit: what words are in a
file, where are they, and how many are there? In this book, we'll build a small
text analyzer for those questions, starting with word counts.

## From functions to queries

In ordinary Rust, we could write:

```rust
struct File {
    text: String,
}

fn words(file: &File) -> Vec<String> {
    file.text.split_whitespace().map(str::to_owned).collect()
}

fn word_count(file: &File) -> usize {
    words(file).len()
}
```

Each call to `word_count` runs `words` again, even if the text hasn't changed.
With Ajika, we can describe the text as an input and these functions as queries:

```rust
# extern crate ajika;
{{#rustdoc_include ../../examples/word-count/src/main.rs:declarations}}
```

The function bodies are still ordinary Rust. The extra declarations let Ajika
remember which inputs and other query answers each function used.

Suppose we change `hello world` to `hello    world`, widening the gap to four
spaces. Ajika reruns `words` because the text changed, but its answer is still
`["hello", "world"]`. Since `word_count` depends on that vector, Ajika can reuse
the count without running its function again. Counting words is cheap, but the
same boundary could protect an expensive analysis from unnecessary work.

## Reuse across requests and restarts

With a disk database, Ajika provides **persistent incrementality**: it saves query
results and their dependencies, so later requests can validate and reuse those
results even after a restart.

Ajika is inspired by [Salsa](https://github.com/salsa-rs/salsa), with a different
emphasis on where reusable work lives. Salsa is memory-first, while Ajika is
designed to serve requests from a disk cache. It loads query values for an
operation, then releases its working data when the operation ends. Large results
can stay reusable on disk without occupying RAM between requests.

You can also keep reusable work in RAM with a memory database. For frequent
unsaved edits, a session overlay can reuse results in memory while preserving
saved answers on disk. The query functions stay the same in each case; the
following chapters show how to choose where their results live.

## How to read this book

The [tutorial](using/first-queries.md) turns the queries above into a runnable
program. We'll supply text, edit it, reuse results after reopening, and separate
words from their source locations. Later chapters cover interning, loading inputs
on demand, unsaved edits, parallel work, and memory and storage budgets.

The [conceptual reference](concepts/overview.md) explains the choices behind the
public interface: what belongs in an input or a query key, what callers depend on,
how to organize a request, and what to keep between requests. Read it when designing
your own integration, or return to individual chapters as those questions arise.
It does not require following the tutorial.

The [internal walkthrough](internals/overview.md) follows query calls through the
implementation. It introduces the objects after explaining the problem they solve,
then traces validation, publication, and storage.

## Where the boundary lies

Ajika manages derived work. Your application still decides what counts as an input,
when those inputs form a coherent view, and when a result is fit to publish to a user.
Source files, configuration, and editor buffers remain application concerns.

Ajika's disk store is a disposable cache. It is not the authoritative copy of those
inputs, a general database for user data, or a way to query arbitrary historical
revisions. A corrupted or incompatible cache can be discarded and rebuilt.

The project is under active development. APIs and cache formats can change. The
examples here are checked against the library in this repository.

## About this book

This book is also inspired by [the Salsa book](https://salsa-rs.github.io/salsa/).
