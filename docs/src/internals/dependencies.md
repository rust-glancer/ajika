# Remembering what a query read

Ajika does not ask query authors to list their dependencies. It records observations
as they happen. This matters because a query may choose different children for
different input values.

The observation must belong to the **calling query**, even if another worker
computed the value or storage supplied it. Sharing a value does not mean sharing
the caller's dependency log.

## Direct reads preserve useful boundaries

In the word-count example:

```text
words frame:       File::text("notes.txt") at stamp S1
word_count frame:  words("notes.txt") at version W1
```

The parent does not inherit the child's text dependency. If it did, a whitespace
edit would invalidate the parent directly and there would be no chance for equal
words to stop that propagation.

[`Db`](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/runtime/mod.rs#L434) carries both the shared operation and the active `Frame`.
When the evaluator enters a child function, it creates a child frame. After the
child is ready, the read is recorded on the caller's frame using the child's output
version.

An input getter records the field address and stamp instead. An `Output::value`
read records the producer address, selected slot, and that slot's version.
These cases become the two variants of [`Dependency`](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/runtime/record.rs#L82).

The frame also accumulates the least durability and lifetime of its observations.
Those policies flow transitively through children without copying their entire
dependency lists into the parent.

## The order of reads matters

Suppose a query first reads which file is selected, then reads that file:

```text
old execution: selection → file A
new execution: selection → file B
```

If selection changes, validation must stop there and let the function run again.
Trying to validate every old dependency first could access file A after it has
been deleted, even though the new computation would never read it.

[`Reads::depend`](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/runtime/frame.rs#L187) keeps the first-read order and deduplicates
repeated observations. Validation examines that sequence and stops at the first
mismatch.

When a new execution completes, its reads replace the old list. Even if A and B
contain equal text and the final answer stays equal, future validation must follow
B. Output equality preserves a version, not the old explanation of that version.

## Repeated acquisition within a function

A function may ask for the same child or output many times. Its frame remembers
already acquired values, so later reads can return the value without repeating
acquisition or adding the same dependency again.

Now suppose two different queries call `words("notes.txt")`. The operation's memo
lets both receive the same decoded vector, but each caller's frame must record
its own dependency on W1. Reusing the first caller's log would leave the second
query with no record of that read.

At the host root, the frame has no query owner. Host reads do not create another
stored producer, though queries called by the host still track their own reads.

## Parallel branches are parts of their caller

`parallel_map` gives each callback its own frame with the same query owner.
Workers can collect reads independently without racing to append a single log.
After joining, [branch merging](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/runtime/frame.rs#L200) combines their logs in input order,
deduplicating shared observations.

The callbacks are not separate persistent queries. If each item needs its own
reuse boundary, call a declared query from the callback.

At the end of execution, [`Frame::finish`](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/runtime/frame.rs#L129) transfers the collected
reads and pending outputs to the evaluator. It does not write storage. The
evaluator still has to compare answers and publish the completed record.
