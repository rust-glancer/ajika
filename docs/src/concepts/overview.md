# The application's model

Ajika lets an application ask the same questions as its data changes. A request
might ask for a module's declarations, an image's thumbnail, or the diagnostics
for one function. The application describes how to compute the answer; Ajika
decides when a previous answer is still usable.

This section explains the public concepts and the choices they support. You can
read it without the tutorial or knowledge of the runtime's implementation. The
type and method names give you entrypoints to look up in the API documentation
when you need exact signatures.

## Separate supplied facts from derived answers

An **input** is a fact supplied by the application: file text, language settings,
or the list of files in a project. A **query** computes an answer by reading inputs
and calling other queries.

For example, a declaration query can read source text and an edition setting.
Ajika remembers those observations. Editing another file doesn't necessarily
affect that answer. An edit to this file requires checking it, but may still
leave its declarations equal.

The question needs an identity that survives those edits. A **query key** says
which question is being asked—perhaps a file and its project context. The changing
text belongs in an input. Otherwise every edit would become an unrelated question,
with no previous answer to compare.

The declaration macros express those roles:

| Declaration | Its role in the application |
| --- | --- |
| `#[ajika::input]` | Data the host can change and queries can observe. |
| `#[ajika::query]` | A computation whose answer Ajika may reuse. |
| `#[ajika::interned]` | Compact IDs for immutable values reused across keys and results. |
| `#[ajika::output]` | Handles for separately readable results produced together. |

They are useful independently. A query can use ordinary keys and return one
ordinary value. It doesn't need interned IDs or output handles.

## Give the host and queries different jobs

The host owns a long-lived `Database`. It supplies inputs and starts an
**operation** with `Database::run`. The operation is a scope of work such as
serving an editor request.

The callback and its queries receive a `Db`. It connects their reads to the
installed inputs and lets queries share work. Passing it to a child query is how
the child participates in the same operation.

```text
host publishes inputs
        ↓
Database::run
        ├─ requested query
        │    ├─ input reads
        │    └─ child queries
        └─ result returned to the host
```

Ordinary query calls return `Arc<T>`. Callers can work with the answer without
knowing whether it was just computed or reused. When the operation finishes,
Ajika retains eligible results for later use and releases its temporary working
set. The host decides what to publish to the user.

## Choose the part you need

- [Inputs, interning, and context](inputs-and-interning.md) explains how compact IDs,
  supplied values, and project settings work together.
- [Queries and observable change](queries.md) explains where reuse boundaries
  belong and what makes an answer safe to reuse.
- [Values and output handles](results.md) explains the difference between keeping
  an answer and keeping a way to ask for it again.
- [Operations](operations.md) explains how requests, input updates, and failures fit
  into application control flow.
- [Where results live](residency.md) separates memory sharing, persistence, session
  reuse, and eviction.
- [Owning the cache](persistence.md) covers startup, schema changes, and maintenance.

The API documentation gives the detailed contracts for these entrypoints. From a
checkout, `cargo doc -p ajika --no-deps --open` opens that documentation for the
version you are using.
