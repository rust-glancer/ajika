# Working in this repository

- Keep responses short. State the outcome and any remaining limitation once.
- Humans create commits and pull requests; do not create them on the user's behalf.
- Ask before adding dependencies. Declare dependencies in the workspace manifest
  and enable features in each consuming crate.
- Use `mod.rs` for modules with submodules.
- Prefer private visibility, then `pub(crate)` for crate internals and `pub` for
  public API. Avoid `pub(in ...)`.
- Group operations with the types that own them. Avoid single-use helpers and
  argument objects that only silence lints.
- Explain non-obvious reasoning beside the relevant code. Write for a reader
  without prior context; preserve useful existing explanations.
- Prefer `expect` with a concise invariant over `unwrap`. Add error context only
  when it helps identify a distinct failure.
- Preserve low idle memory. Temporary working memory during an operation is expected.
- APIs and cache formats are experimental. Prefer a clear design over compatibility
  adapters that have no real consumer.
- Test observable behavior with small fixtures and explicit examples. Do not add
  adversarial tests unless requested.
- `just test` includes doctests and the separately linked release replay fixture.
  Choose focused checks while editing and run appropriate checks before finishing.
- Run `just lint` and `cargo dylint` outside filesystem sandboxes that prevent
  Dylint from reading build metadata.
- `reference/` is ignored local research. Do not reference its contents from source.
