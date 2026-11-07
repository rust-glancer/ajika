# Working in this repository

- Humans create commits and pull requests; do not create them on the user's behalf.
- Ask before adding dependencies. Declare dependencies in the workspace manifest
  and enable features in each consuming crate.
- Use `mod.rs` for modules with submodules.
- Prefer private visibility, then `pub(crate)` for crate internals and `pub` for
  public API. Avoid `pub(in ...)`.
- Explain non-obvious reasoning beside the relevant code. Write for a reader
  without prior context; preserve useful existing explanations.
- Prefer `expect` with a concise invariant over `unwrap`. Add error context only
  when it helps identify a distinct failure.
- Preserve low idle memory. Temporary working memory during an operation is expected.
- APIs and cache formats are experimental. Prefer a clear design over compatibility
  adapters that have no real consumer.
- Run `just lint` and `cargo dylint` outside filesystem sandboxes that prevent
  Dylint from reading build metadata.

## Avoid single-use helpers

Instead of introducing single-use helpers, prefer embedding functionality as a block with comment.
Bad (if only used once):
```
fn collapse_whitespace(text: String) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
```
Good:
```
// Ensure that all the whitespaces are normal " ".
text = text.split_whitespace().collect::<Vec<_>>().join(" ")
```

## Tests

- Test observable behavior with small fixtures and explicit examples. Do not add
  adversarial tests unless requested.
- We don't aim to have 100% test coverage, we aim to have tests that _make sense_.
  Something that is only possible under a manufactured input, as if user tries to
  shoot themselves into a foot, is not worth pinning. Behavior that does not matter,
  e.g. small implementation details, are also not worth pinning.
- Good tests are ones that survive refactoring. Unless you test public API, do not
  overly focus on the API contracts. Focus on the _observable_ behavior.

## Comments

Add simple-to-read comments in logically complex blocks to help the reader see what's going on.
Where reasonable, use small examples driven by Rust syntax.
Typically, functions with real business logic deserve at least a short comment.
Simple getters, constructors, and obvious wrappers usually do not. Same goes for types.
Inside of functions, comments might be helpful to explain an intention or non-trivial
block of logic. When the function represents multiple logical steps, walkthrough-style
comments are usually a good idea.

Comment priorities: clarity first, size second. The comment might be slightly obvious
if it helps to understand what's going on, but we don't need to explain the entire codebase
all the time.

The goal of comments is to reduce cognitive complexity and help read the code as a book.
Prefer commenting what exists, not cross-reference.
Avoid documenting things that can go stale quickly and do NOT help reading the code, e.g.
the scope of the task you're working on, other files/modules that use this function,
temporary design decisions.

Rules of thumb:
- reader should not have special knowledge to understand the comment (e.g. project roadmap/tasks/private discussions)
- reader should learn something from the comment that otherwise would require them to spend time reasoning about codebase.
- if your comment uses the word "currently" or another implication of current state that is likely to change, then it's probably a temporary implementation detail that should not be mentioned.

### Documentation Voice

In this project, we favor plain pre-reading notes over polished reference-doc summaries.
The docs may be step-by-step, slightly repetitive, or intentionally simple if that helps the next
reader understand the code before reading it.

When proofreading existing comments, preserve the author's structure and voice. Do not compress,
formalize, or "upgrade" comments into academic Rustdoc style unless explicitly asked. Fix only the
parts that are unclear, grammatically broken, or misleading.

A lot of existing documentation may still read too polished or bland, this is a bug, not a feature.
Do not treat overly concise/compressed docs as a model to match, since we try to improve situation
by making new documentation better than what we had, not create a uniform mix between "old" and
"new" styles. Think of documentation in this project as being in the stage of slow and gradual
rewrite spanning multiple months.

A good comment should pass this test: can a tired reader understand why the next code exists before
they read the code? If not, make the comment more concrete, not more elegant.
