//! Markdown work without a query database. A second incremental engine can use this unchanged.

use std::collections::BTreeSet;

use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use serde::Serialize;
use url::Url;
use wincode::{SchemaRead, SchemaWrite};

/// One scan produces several facts that do not necessarily change together.
pub(crate) struct ParsedDocument {
    pub(crate) anchors: Vec<String>,
    pub(crate) links: Vec<Link>,
    pub(crate) locations: Vec<Location>,
    pub(crate) html: String,
}

impl ParsedDocument {
    pub(crate) fn parse(path: &str, text: &str) -> Self {
        let base = Target::page_url(path);
        let mut anchors = BTreeSet::new();
        let mut links = Vec::new();
        let mut locations = Vec::new();
        let mut events = Vec::new();
        let mut heading: Option<(usize, String)> = None;
        let line_starts: Vec<_> = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(offset, _)| offset + 1))
            .collect();

        // Keep the parser's event stream only for this scan. Persist the resulting HTML,
        // rather than a parser-specific tree that every preview would have to reconstruct.
        for (mut event, range) in Parser::new(text).into_offset_iter() {
            match &mut event {
                Event::Start(Tag::Heading { .. }) => {
                    heading = Some((events.len(), String::new()));
                }
                Event::Text(value) | Event::Code(value) => {
                    if let Some((_, title)) = &mut heading {
                        title.push_str(value);
                    }
                }
                Event::SoftBreak | Event::HardBreak => {
                    if let Some((_, title)) = &mut heading {
                        title.push(' ');
                    }
                }
                Event::End(TagEnd::Heading(_)) => {
                    let (start, title) = heading.take().expect("balanced Markdown heading");
                    let base = Self::slug(&title);
                    let mut anchor = base.clone();
                    let mut occurrence = 2;
                    while !anchors.insert(anchor.clone()) {
                        anchor = format!("{base}-{occurrence}");
                        occurrence += 1;
                    }
                    if let Event::Start(Tag::Heading { id, .. }) = &mut events[start] {
                        *id = Some(anchor.into());
                    }
                }
                Event::Start(Tag::Link { dest_url, .. }) => {
                    let href = dest_url.to_string();
                    let target = Target::resolve(&base, &href);
                    let line = line_starts.partition_point(|&start| start <= range.start);
                    locations.push(Location {
                        start: range.start,
                        end: range.end,
                        line,
                        column: text[line_starts[line - 1]..range.start].chars().count() + 1,
                    });
                    // Exported pages use .html links. The preview UI intercepts these same
                    // links to open the corresponding Markdown document in the editor.
                    *dest_url = target.render_href(&base, &href).into();
                    links.push(Link { href, target });
                }
                // Authored HTML stays visible as text. The example has no asset pipeline;
                // images retain their alt text but do not load local or remote resources.
                Event::Html(value) | Event::InlineHtml(value) => {
                    event = Event::Text(value.clone());
                }
                Event::Start(Tag::Image { .. }) | Event::End(TagEnd::Image) => continue,
                _ => {}
            }
            events.push(event);
        }
        let mut html = String::new();
        pulldown_cmark::html::push_html(&mut html, events.into_iter());
        Self {
            anchors: anchors.into_iter().collect(),
            links,
            locations,
            html,
        }
    }

    fn slug(title: &str) -> String {
        let slug = title
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        if slug.is_empty() {
            "section".into()
        } else {
            slug
        }
    }
}

#[derive(Debug, PartialEq, Eq, SchemaRead, SchemaWrite)]
pub(crate) struct Link {
    pub(crate) href: String,
    pub(crate) target: Target,
}

#[derive(Debug, PartialEq, Eq, SchemaRead, SchemaWrite)]
pub(crate) enum Target {
    Page { path: String, anchor: String },
    External,
    Unsupported,
}

impl Target {
    fn page_url(path: &str) -> Url {
        // A virtual origin gives document paths normal URL resolution without filesystem
        // access. Push path segments so spaces, #, and ? in filenames are encoded as data.
        let mut url = Url::parse("https://handbook.invalid/").expect("literal URL");
        url.path_segments_mut()
            .expect("hierarchical URL")
            .extend(path.split('/'));
        url
    }

    fn resolve(base: &Url, href: &str) -> Self {
        let Ok(url) = base.join(href) else {
            return Self::Unsupported;
        };
        if url.origin() != base.origin() || Url::parse(href).is_ok() {
            return if matches!(url.scheme(), "http" | "https" | "mailto") {
                Self::External
            } else {
                Self::Unsupported
            };
        }
        if url.query().is_some() {
            return Self::Unsupported;
        }
        // Query keys use the decoded source filename. Decode per segment so an encoded
        // separator cannot introduce a second interpretation of the same path.
        let parts: Option<Vec<_>> = url
            .path_segments()
            .expect("handbook URL")
            .map(|part| {
                let decoded = percent_encoding::percent_decode_str(part)
                    .decode_utf8()
                    .ok()?;
                (!decoded.contains(['/', '\\'])).then(|| decoded.into_owned())
            })
            .collect();
        let Some(parts) = parts else {
            return Self::Unsupported;
        };
        let path = parts.join("/");
        let Ok(anchor) =
            percent_encoding::percent_decode_str(url.fragment().unwrap_or("")).decode_utf8()
        else {
            return Self::Unsupported;
        };
        if !path.ends_with(".md") {
            return Self::Unsupported;
        }
        Self::Page {
            path,
            anchor: anchor.into_owned(),
        }
    }

    fn render_href(&self, base: &Url, original: &str) -> String {
        match self {
            Self::Page { path, anchor } => {
                let stem = path.strip_suffix(".md").expect("Markdown target");
                let mut target = Self::page_url(&format!("{stem}.html"));
                target.set_fragment((!anchor.is_empty()).then_some(anchor));
                base.make_relative(&target).expect("same handbook origin")
            }
            Self::External => original.to_owned(),
            Self::Unsupported => "#".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, SchemaRead, SchemaWrite)]
pub(crate) struct Location {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) line: usize,
    pub(crate) column: usize,
}
