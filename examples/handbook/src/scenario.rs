//! A repeatable editing session using the same application host as the browser.

use std::{fs, path::Path};

use anyhow::Result;

use crate::{
    host::{Host, Run},
    queries::Page,
};

pub(crate) const FILES: &[(&str, &str)] = &[
    ("index.md", include_str!("../content/index.md")),
    (
        "installation.md",
        include_str!("../content/installation.md"),
    ),
    (
        "configuration.md",
        include_str!("../content/configuration.md"),
    ),
    (
        "guides/observing.md",
        include_str!("../content/guides/observing.md"),
    ),
];

pub(crate) fn copy_handbook(root: &Path) -> Result<()> {
    for (path, text) in FILES {
        let destination = root.join(path);
        fs::create_dir_all(destination.parent().expect("fixture parent"))?;
        fs::write(destination, text)?;
    }
    Ok(())
}

pub(crate) fn run(workers: usize) -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path().join("content");
    let cache = temporary.path().join("cache");
    copy_handbook(&root)?;
    println!(
        "Each step builds all pages. The fixture is deliberately small; timings are illustrative."
    );
    println!("step                 execute validate decode payload-KiB metadata-KiB issues     ms");
    let mut host = Host::open(&root, Some(&cache), workers)?;
    report("cold build", host.build()?);

    drop(host);
    let mut host = Host::open(&root, Some(&cache), workers)?;
    report("reopen saved cache", host.build()?);

    let installation = fs::read_to_string(root.join("installation.md"))?;
    let paragraph = installation.replace("warm clothes", "a warmer coat");
    fs::write(root.join("installation.md"), &paragraph)?;
    host.refresh()?;
    report("change a paragraph", host.build()?);

    let renamed = paragraph.replace("## Requirements", "## What to bring");
    fs::write(root.join("installation.md"), &renamed)?;
    host.refresh()?;
    report("rename a heading", host.build()?);

    // Repair the heading in an unsaved buffer. Incoming links observe this draft too.
    host.edit("installation.md", paragraph)?;
    report("preview draft", host.build()?);
    report("repeat preview", host.build()?);
    host.save()?;
    report("save draft", host.build()?);
    println!(
        "The demo used temporary files. Run 'handbook serve' to try these edits in the browser."
    );
    Ok(())
}

fn report(label: &str, run: Run<Vec<Page>>) {
    let issues: usize = run.value.iter().map(|page| page.diagnostics.len()).sum();
    let work = run.work;
    println!(
        "{label:21} {:7} {:8} {:6} {:11.1} {:12.1} {issues:6} {:6.1}",
        work.executed,
        work.validated,
        work.decoded,
        work.bytes_written as f64 / 1024.0,
        work.metadata_bytes_written as f64 / 1024.0,
        work.milliseconds
    );
}
