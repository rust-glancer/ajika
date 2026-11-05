# Handbook

A small documentation compiler with a single-user editor. The bundled handbook
introduces volunteers to a hilltop observatory: what to bring, how to find it,
and how to begin observing. Its pages link to one another.

Open a page, edit its Markdown, and preview the result without saving. Build the
whole handbook to check its links in parallel. Stop and restart the application
to reuse saved computations.

## Try it

From the workspace root:

```sh
just handbook serve
```

Open the printed address, normally `http://127.0.0.1:3131`. The editor shows a page
selector, source text, rendered preview, link diagnostics, and the work performed
by the latest operation. The server binds to loopback only.

`just handbook --help` lists the commands; `just handbook serve --help` describes
the editor's options.

Save changes the actual Markdown files. To explore without changing the bundled
handbook, copy `examples/handbook/content` elsewhere and pass that directory:

```sh
just handbook serve /tmp/my-handbook --workers 4
```

Or run the complete editing story on a temporary copy:

```sh
just handbook demo --workers 4
```

The demo prints one row per operation: executions, validations, decoded values,
payload and metadata bytes written, diagnostic count, and elapsed time.

## Follow an edit

First click **Build handbook**. The application asks for all four pages through
`Db::parallel_map`. Several workers need `installation.md`'s anchors, but its
analysis runs once. Worker IDs in the execution table show where work ran.
Repeated builds can validate or load results without executing their providers.

Open `installation.md` and change “warm clothes” to “a warmer coat”. The preview
updates after a short typing pause. Build again: the other pages still link to
exactly the same headings, so their link checks stay reusable.

Now rename `## Requirements` to `## Packing list` and build. Three incoming links
become broken, even though the draft has not been saved. These are ordinary
query results, not failed database operations. Click a diagnostic to find its
source. Switching documents asks you to discard an active draft; **Save** keeps it.

To see source locations change independently, open `index.md`, change its target
to `installation.md#missing`, and preview it. Add two blank lines above the
link. The diagnostic moves down two lines, but “check links” does not execute
again: only the source location changed.

Repeated previews use the same session results. **Save** writes the text and
publishes it as persistent input; retained preview work can be promoted without
recomputation. **Discard** returns to the latest saved text. An external editor's
changes arrive through the filesystem watcher. They update the saved baseline
without replacing an active draft; Save reports a conflict if its original file
changed externally.

Finally, stop and restart the server, then build again. Ajika validates saved
results against freshly captured sources. The original parse/check computations
need not run. The same is exercised without the browser by `demo`.

## Why the queries are shaped this way

[Analysis::for_document](src/queries.rs) parses one source and emits four named
outputs:

| Output | Consumer |
| --- | --- |
| Anchors, or absence for a missing document | Incoming link checks |
| Outgoing link targets | That document's link check |
| Link source locations | Diagnostic placement |
| HTML fragment | Preview and export |

A paragraph and a heading are different observations. If all of them lived in one
large query value, changing any text would invalidate every consumer. Named
outputs let the scan stay together while each consumer reads the fact it needs.

`check_links` reads the outgoing links and each target's anchors. It never asks
a target to check its own links, so circular hyperlinks do not create query
cycles. A missing target is a tracked absence: adding the file can repair a link
without editing its source.

`diagnostics` attaches positions to the issues found by `check_links`. If there
are no issues, it has no reason to depend on positions. [Page::acquire](src/queries.rs)
loads the HTML and diagnostics for a response. This small composition is an
ordinary function; storing another aggregate page would duplicate its HTML.

[Host](src/host.rs) owns one saved input snapshot and one optional draft. A draft
is a sparse overlay with `Lifetime::Session`. Queries use the same document IDs
and functions for saved and unsaved text. Saving changes the input's retention
policy while preserving its content stamp. The host writes files; a memoized
query cannot own that side effect because a cache hit skips execution.

Inputs and operations change sequentially on the host. Parallelism lives inside
a build, where workers share the active input view and query computations. Each
successful operation checkpoints eligible work and releases its working values.
The response owns any returned HTML until it has been sent or exported.

## Other commands

```sh
# Export HTML, without starting a server.
just handbook build --out target/handbook

# Use the same application with Ajika's in-memory backend.
just handbook serve --memory

# Choose another cache directory or port.
just handbook serve --cache /tmp/handbook-cache --port 3132

cargo nextest run -p ajika-handbook --locked
```

Without `--cache`, saved queries live under the source directory's hidden
`.handbook-cache` folder. Removing that folder resets the example's derived work.
`--workers 1` uses the same query graph sequentially. Exports preserve relative
links, replacing `.md` destinations with `.html`; removed pages are not pruned
from an existing export directory. A build with link diagnostics still writes its
HTML and reports the issues, then exits with a failure status.

## Reading the implementation

Start with [the walkthrough](src/scenario.rs) or [behavior tests](src/tests.rs),
then read [the queries](src/queries.rs). The tests cover restart reuse, shared
parallel work, selective invalidation, diagnostic relocation, and draft/save
behavior using the same host as the server.

[document.rs](src/document.rs) does the Markdown work without an Ajika context.
[host.rs](src/host.rs) captures sources and owns application operations.
[web/mod.rs](src/web/mod.rs) translates HTTP commands and watcher notifications;
[editor.html](src/web/editor.html) is plain HTML and JavaScript.

Saving uses a temporary file and atomic replacement, preserving the original
file's permissions.

## Example scope

Markdown uses `pulldown-cmark`'s CommonMark parser. Handbook links use relative
or handbook-root-relative `.md` paths and optional heading fragments. URL encoding
works for filenames and fragments, including spaces and Unicode. Headings get lowercase,
hyphen-separated anchors; repeated headings get `-2`, `-3`, and so on.
HTTP, HTTPS, and mail links are rendered but not fetched or checked.
Query strings, assets, and raw HTML are outside the example:
unsupported links produce diagnostics, raw HTML is escaped, and images show
only their alt text. Sources must be UTF-8, and symlinks and hidden files are skipped.

The editor has one shared draft, not independent browser sessions. Source capture
rescans the small directory after a burst of watcher events and retains captured
text. Save checks for external changes but does not coordinate writes with other
editors.
The browser sends a complete document per edit, with a 2 MiB request limit.

The measured operation includes query work and checkpointing, but excludes source
capture, HTTP/JSON work, and HTML export.
A zero-execution request can still validate dependencies or decode saved values;
the other counters make that work visible. Byte counters describe encoded
payloads and query metadata, not total filesystem allocation or RAM.
