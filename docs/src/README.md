# About Ajika

Ajika is a library that lets your application to remember computations and reuse
them when inputs do not change. Moreover, it lets you to store these computations
to the disk, allowing you to offload computations from memory when they are not
needed, load them on demand, and reuse computations across the application restarts.

In other words, Ajika is a **persistent incrementality** library.

Ajika is heavily inspired by [Salsa](https://github.com/salsa-rs/salsa), with
a different focus. Salsa is generally oriented to provide maximum efficiency, and
is memory-first. Ajika is the opposite: it accepts a performance hit (often not
very big in practice, especially for heavy computations) to offload data _from_
memory and load it from disk on demand. It might be a good choice if you want to
use Salsa approach but reduce the memory usage.

You can also keep reusable work in RAM with a memory database. For frequent
unsaved edits, a session overlay can reuse results in memory while preserving
saved answers on disk. The query functions stay the same in each case; the
following chapters show how to choose where their results live.

## LLM usage disclaimer

This book was written with use of LLMs, but it has plenty a human touch.
I've spend hours on this text to make sure that it's something I am comfortable
reading myself, so, I hope, it will be a good read for you too.

Regardless, I am very open for feedback, so if some parts (or even the book as a whole)
do not feel good, please [open an issue](https://github.com/rust-glancer/ajika/issues)
and I'll work on it.

Eventually I do plan to rewrite the documentation myself from scratch, but it's a very
big amount of work and there are only so many hours in a day.

## About this book

This book tries to teach you Ajika through a prism of a very naive query in a
text editor. The request is very stupid: how many words are there in the document.

In real world, the queries are typically much heavier, when your task
is implementing an LSP server, a compiler, or a build system. The editor we're
discussing is, obviously, not a project where Ajika shines (because with such simple queries
the persistence overhead will dominate, and the memory usage is low anyways), but a project
that is useful for understanding basic concepts.

Regardless, let's think how we would implement such a request.

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

The project is under active development. APIs and cache formats can change.

## About this book

This book is obviously inspired by [the Salsa book](https://salsa-rs.github.io/salsa/).
