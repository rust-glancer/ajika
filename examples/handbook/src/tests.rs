use std::fs;

use anyhow::Result;

use crate::{
    document::{ParsedDocument, Target},
    host::Host,
    scenario,
};

#[test]
fn markdown_produces_anchors_links_and_html_together() {
    let parsed = ParsedDocument::parse(
        "guide/intro.md",
        "# First *light*\n\n[Setup](../installation.md#requirements)\n\n# First light\n",
    );
    assert_eq!(parsed.anchors, ["first-light", "first-light-2"]);
    assert_eq!(parsed.locations[0].line, 3);
    assert_eq!(
        parsed.html,
        "<h1 id=\"first-light\">First <em>light</em></h1>\n\
         <p><a href=\"../installation.html#requirements\">Setup</a></p>\n\
         <h1 id=\"first-light-2\">First light</h1>\n"
    );
}

#[test]
fn handbook_links_use_url_resolution_and_encoding() {
    for (from, href, path, anchor, rendered) in [
        (
            "guides/intro.md",
            "../installation.md#requirements",
            "installation.md",
            "requirements",
            "../installation.html#requirements",
        ),
        (
            "guides/intro.md",
            "/installation.md#requirements",
            "installation.md",
            "requirements",
            "../installation.html#requirements",
        ),
        (
            "index.md",
            "night%20notes.md#d%C3%A9part",
            "night notes.md",
            "départ",
            "night%20notes.html#d%C3%A9part",
        ),
        (
            "night notes.md",
            "#d%C3%A9part",
            "night notes.md",
            "départ",
            "night%20notes.html#d%C3%A9part",
        ),
        (
            "index.md",
            "notes%20%231.md",
            "notes #1.md",
            "",
            "notes%20%231.html",
        ),
    ] {
        let parsed = ParsedDocument::parse(from, &format!("[Read]({href})"));
        assert_eq!(
            parsed.links[0].target,
            Target::Page {
                path: path.into(),
                anchor: anchor.into(),
            },
            "{from} -> {href}"
        );
        assert!(
            parsed.html.contains(&format!("href=\"{rendered}\"")),
            "{from} -> {href}: {}",
            parsed.html
        );
    }
}

#[test]
fn encoded_links_resolve_to_source_filenames() -> Result<()> {
    let temp = tempfile::tempdir()?;
    fs::write(
        temp.path().join("index.md"),
        "[Notes](night%20notes.md#d%C3%A9part)",
    )?;
    fs::write(temp.path().join("night notes.md"), "# Départ\n")?;
    let mut host = Host::open(temp.path(), None, 2)?;
    assert!(host.preview("index.md")?.value.diagnostics.is_empty());
    Ok(())
}

#[test]
fn parallel_build_and_restart_reuse_the_same_pages() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("source");
    let cache = temp.path().join("cache");
    scenario::copy_handbook(&root)?;
    let mut host = Host::open(&root, Some(&cache), 4)?;
    let first = host.build()?;
    assert_eq!(first.value.len(), scenario::FILES.len());
    assert!(first.value.iter().all(|page| page.diagnostics.is_empty()));
    for (path, _) in scenario::FILES {
        assert_eq!(
            first
                .work
                .executions
                .iter()
                .filter(|event| event.query == "analyze" && event.page == *path)
                .count(),
            1,
            "shared targets are analyzed once"
        );
    }
    drop(host);

    let mut host = Host::open(&root, Some(&cache), 1)?;
    let reopened = host.build()?;
    assert_eq!(reopened.work.executed, 0, "saved results survive reopening");
    for (before, after) in first.value.iter().zip(&reopened.value) {
        assert_eq!(
            before.html, after.html,
            "worker count does not change output"
        );
        assert_eq!(before.diagnostics, after.diagnostics);
    }
    Ok(())
}

#[test]
fn paragraph_edits_do_not_recheck_incoming_links_but_heading_edits_do() -> Result<()> {
    let temp = tempfile::tempdir()?;
    scenario::copy_handbook(temp.path())?;
    let mut host = Host::open(temp.path(), None, 4)?;
    host.build()?;
    let path = temp.path().join("installation.md");
    let text = fs::read_to_string(&path)?;
    fs::write(&path, text.replace("warm clothes", "a warmer coat"))?;
    host.refresh()?;
    let paragraph = host.build()?;
    assert_eq!(paragraph.work.executions.len(), 1);
    assert_eq!(paragraph.work.executions[0].query, "analyze");
    assert!(
        paragraph
            .value
            .iter()
            .any(|page| page.html.contains("warmer coat"))
    );

    fs::write(path, text.replace("## Requirements", "## What to bring"))?;
    host.refresh()?;
    let renamed = host.build()?;
    let messages: Vec<_> = renamed
        .value
        .iter()
        .flat_map(|page| {
            page.diagnostics
                .iter()
                .map(|diagnostic| (page.path.as_str(), diagnostic.message.as_str()))
        })
        .collect();
    assert_eq!(
        messages,
        [
            (
                "configuration.md",
                "Missing heading: installation.md#requirements"
            ),
            (
                "guides/observing.md",
                "Missing heading: installation.md#requirements"
            ),
            ("index.md", "Missing heading: installation.md#requirements"),
        ]
    );
    Ok(())
}

#[test]
fn moving_a_link_updates_its_diagnostic_without_rechecking_it() -> Result<()> {
    let temp = tempfile::tempdir()?;
    fs::write(
        temp.path().join("index.md"),
        "# Welcome\n\n[Future](later.md#arrival)\n",
    )?;
    let mut host = Host::open(temp.path(), None, 2)?;
    assert_eq!(
        host.preview("index.md")?.value.diagnostics[0].location.line,
        3
    );
    fs::write(
        temp.path().join("index.md"),
        "\n\n# Welcome\n\n[Future](later.md#arrival)\n",
    )?;
    host.refresh()?;
    let moved = host.preview("index.md")?;
    assert_eq!(moved.value.diagnostics[0].location.line, 5);
    assert!(
        !moved
            .work
            .executions
            .iter()
            .any(|event| event.query == "check links")
    );

    fs::write(temp.path().join("later.md"), "# Arrival\n")?;
    host.refresh()?;
    assert!(
        host.preview("index.md")?.value.diagnostics.is_empty(),
        "publishing a previously missing input repairs an existing link"
    );
    Ok(())
}

#[test]
fn drafts_are_reused_saved_and_reopened_without_recomputation() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("source");
    let cache = temp.path().join("cache");
    scenario::copy_handbook(&root)?;
    let mut host = Host::open(&root, Some(&cache), 4)?;
    host.build()?;
    let original = fs::read_to_string(root.join("installation.md"))?;
    let edited = original.replace("Requirements", "Packing list");
    host.edit("installation.md", edited.clone())?;
    let draft = host.build()?;
    assert!(draft.value.iter().any(|page| !page.diagnostics.is_empty()));
    assert_eq!(draft.work.bytes_written, 0);
    assert_eq!(draft.work.metadata_bytes_written, 0);
    assert_eq!(fs::read_to_string(root.join("installation.md"))?, original);
    assert_eq!(
        host.build()?.work.executed,
        0,
        "repeated draft requests reuse work"
    );
    host.save()?;
    assert_eq!(fs::read_to_string(root.join("installation.md"))?, edited);
    assert_eq!(
        host.build()?.work.executed,
        0,
        "saving promotes retained answers"
    );
    drop(host);
    let mut host = Host::open(&root, Some(&cache), 4)?;
    assert_eq!(
        host.build()?.work.executed,
        0,
        "promoted answers survive restart"
    );
    Ok(())
}

#[test]
fn watcher_refresh_preserves_the_draft_and_discard_uses_latest_saved_files() -> Result<()> {
    let temp = tempfile::tempdir()?;
    scenario::copy_handbook(temp.path())?;
    let mut host = Host::open(temp.path(), None, 2)?;
    let original = host.text("installation.md")?;
    let draft = original.replace("warm clothes", "a winter coat");
    host.edit("installation.md", draft.clone())?;
    fs::write(
        temp.path().join("installation.md"),
        original.replace("red torch", "red lamp"),
    )?;
    host.refresh()?;
    assert_eq!(
        *host.text("installation.md")?,
        draft,
        "watcher does not overwrite a draft"
    );
    host.discard()?;
    assert!(
        host.preview("installation.md")?
            .value
            .html
            .contains("red lamp")
    );
    assert!(host.state().draft.is_none());
    Ok(())
}
