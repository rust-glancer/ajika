//! The incremental graph: analyze once, check links against anchors, then attach locations.
//! HTML and diagnostics are acquired together at the host boundary, without storing a second
//! aggregate copy of a page. Parallel builds use these same entrypoints as individual previews.

use std::sync::{Arc, Mutex};

use ajika::{Lifetime, Result, Stamp};
use serde::Serialize;
use wincode::{SchemaRead, SchemaWrite};

use crate::document::{Location, ParsedDocument, Target};

pub(crate) type Db<'a> = ajika::Db<'a, Activity>;

#[ajika::interned(namespace = "handbook-documents")]
pub(crate) struct DocumentId {
    path: String,
}

/// Missing files are observations too. A link to one can become valid when a watcher
/// publishes its text, without changing the document containing that link.
#[derive(Clone)]
pub(crate) struct Source {
    pub(crate) text: Option<Arc<String>>,
    pub(crate) lifetime: Lifetime,
}

impl Source {
    fn missing(_key: &DocumentId) -> Result<Self> {
        Ok(Self {
            text: None,
            lifetime: Lifetime::Persistent,
        })
    }

    fn stamp(&self) -> Result<Stamp> {
        Stamp::of_value(&self.text)
    }

    fn lifetime(&self) -> Lifetime {
        self.lifetime
    }
}

#[ajika::input(key = DocumentId)]
pub(crate) struct Document {
    #[input(initialize = Source::missing, stamp = Source::stamp, lifetime = Source::lifetime)]
    source: Source,
}

#[ajika::output]
pub(crate) struct Analysis {
    anchors: Option<Vec<String>>,
    links: Vec<crate::document::Link>,
    locations: Vec<Location>,
    html: String,
}

#[ajika::query]
impl Analysis {
    #[ajika::query]
    pub(crate) fn for_document(cx: &Db<'_>, document: DocumentId) -> Result<Self> {
        let path = document.path(&cx.interner())?;
        cx.services().record("analyze", &path);
        let source = Document::at(document).source(cx)?;
        let parsed =
            ParsedDocument::parse(&path, source.text.as_deref().map_or("", String::as_str));
        Self::new(
            cx,
            source.text.map(|_| parsed.anchors),
            parsed.links,
            parsed.locations,
            parsed.html,
        )
    }
}

/// The link's position in the analysis output is stable when only whitespace moves.
/// Keeping byte positions out of this result stops those edits at the location boundary.
#[derive(Debug, PartialEq, Eq, SchemaRead, SchemaWrite)]
struct Issue {
    link: usize,
    message: String,
}

#[ajika::query]
fn check_links(cx: &Db<'_>, document: DocumentId) -> Result<Vec<Issue>> {
    let path = document.path(&cx.interner())?;
    cx.services().record("check links", &path);
    let links = Analysis::for_document(cx, document)?.links.value(cx)?;
    let mut issues = Vec::new();
    for (link, reference) in links.iter().enumerate() {
        let message = match &reference.target {
            Target::External => continue,
            Target::Unsupported => format!("Unsupported handbook link: {}", reference.href),
            Target::Page { path, anchor } => {
                let target = DocumentId::new(&cx.interner(), path.clone())?;
                // A hyperlink cycle is not a query cycle: do not ask the other page to
                // check its links. Only its anchors affect the answer to this question.
                let anchors = Analysis::for_document(cx, target)?.anchors.value(cx)?;
                match anchors.as_ref() {
                    None => format!("Missing document: {path}"),
                    Some(anchors) if !anchor.is_empty() && !anchors.contains(anchor) => {
                        format!("Missing heading: {path}#{anchor}")
                    }
                    Some(_) => continue,
                }
            }
        };
        issues.push(Issue { link, message });
    }
    Ok(issues)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, SchemaRead, SchemaWrite)]
pub(crate) struct Diagnostic {
    pub(crate) location: Location,
    pub(crate) message: String,
}

#[ajika::query]
fn diagnostics(cx: &Db<'_>, document: DocumentId) -> Result<Vec<Diagnostic>> {
    let path = document.path(&cx.interner())?;
    cx.services().record("locate diagnostics", &path);
    let issues = check_links(cx, document)?;
    if issues.is_empty() {
        return Ok(Vec::new());
    }
    let locations = Analysis::for_document(cx, document)?.locations.value(cx)?;
    Ok(issues
        .iter()
        .map(|issue| Diagnostic {
            location: locations[issue.link].clone(),
            message: issue.message.clone(),
        })
        .collect())
}

#[derive(Debug, Serialize)]
pub(crate) struct Page {
    pub(crate) path: String,
    pub(crate) html: Arc<String>,
    pub(crate) diagnostics: Arc<Vec<Diagnostic>>,
}

impl Page {
    pub(crate) fn acquire(cx: &Db<'_>, document: DocumentId) -> Result<Self> {
        Ok(Self {
            path: document.path(&cx.interner())?,
            html: Analysis::for_document(cx, document)?.html.value(cx)?,
            diagnostics: diagnostics(cx, document)?,
        })
    }

    pub(crate) fn document(&self) -> String {
        let mut title = String::new();
        pulldown_cmark::html::push_html(
            &mut title,
            std::iter::once(pulldown_cmark::Event::Text(self.path.as_str().into())),
        );
        format!(
            "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width\">\
             <title>{}</title><style>body{{max-width:48rem;margin:3rem auto;\
             padding:0 1rem;font:17px/1.6 system-ui}}pre{{overflow:auto}}</style>{}</html>",
            title, self.html
        )
    }
}

/// Observation only: queries never read these events to decide their answers.
/// The UI and walkthrough use them to show which computations actually executed.
#[derive(Default)]
pub(crate) struct Activity(Mutex<Vec<Execution>>);

impl Activity {
    fn record(&self, query: &'static str, page: &str) {
        self.0.lock().expect("activity log").push(Execution {
            query,
            page: page.to_owned(),
            worker: format!("{:?}", std::thread::current().id()),
        });
    }

    pub(crate) fn take(&self) -> Vec<Execution> {
        let mut events = std::mem::take(&mut *self.0.lock().expect("activity log"));
        events.sort_by(|a, b| (&a.page, a.query).cmp(&(&b.page, b.query)));
        events
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct Execution {
    pub(crate) query: &'static str,
    pub(crate) page: String,
    pub(crate) worker: String,
}
