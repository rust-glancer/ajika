# Summary

[About Ajika](README.md)

# Using Ajika

- [Your first queries](using/first-queries.md)
- [Changing inputs](using/inputs.md)
- [Keeping results across restarts](using/persistence.md)
- [Separating outputs](using/outputs.md)
- [Choosing query boundaries](using/queries.md)
- [Interning objects](using/interning.md)
- [Acquiring inputs on demand](using/lazy-inputs.md)
- [Working with unsaved edits](using/sessions.md)
- [Parallel work, errors, and cancellation](using/operations.md)
- [Understanding cost and reuse](using/diagnostics.md)

# Conceptual reference

- [The application's model](concepts/overview.md)
- [Inputs, interning, and context](concepts/inputs-and-interning.md)
- [Queries and observable change](concepts/queries.md)
- [Values and output handles](concepts/results.md)
- [Operations and application state](concepts/operations.md)
- [Where results live](concepts/residency.md)
- [Owning the cache](concepts/persistence.md)

# Inside Ajika

- [Follow one result](internals/overview.md)
- [Calls, addresses, and registration](internals/registration.md)
- [Input snapshots and revisions](internals/inputs.md)
- [Remembering what a query read](internals/dependencies.md)
- [Deciding whether to execute](internals/evaluation.md)
- [Publishing independent outputs](internals/outputs.md)
- [Coordinating workers](internals/coordination.md)
- [Checkpointing an operation](internals/checkpoints.md)
- [Storage and reclamation](internals/storage.md)
