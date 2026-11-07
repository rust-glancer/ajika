//! A small source analyzer, without a parser dependency dominating the runtime costs.
//!
//! Files contain whitespace-separated names. Parsing is transient; names and locations are
//! separately persistent outputs. An index joins per-file summaries, so a one-file edit can
//! stop at an equal answer or propagate through just that file and the aggregate.

use std::sync::Arc;

use ajika::{Db, Inputs, Lifetime, Stamp};

#[derive(Clone)]
pub(crate) struct Source {
    pub(crate) text: Arc<String>,
    pub(crate) lifetime: Lifetime,
}

impl Source {
    fn stamp(&self) -> ajika::Result<Stamp> {
        Ok(Stamp::of(self.text.as_bytes()))
    }

    fn lifetime(&self) -> Lifetime {
        self.lifetime
    }
}

#[ajika::input(key = u32)]
pub(crate) struct File {
    #[input(stamp = Source::stamp, lifetime = Source::lifetime)]
    source: Source,
}

#[ajika::input(key = ())]
pub(crate) struct Project {
    files: Arc<Vec<u32>>,
    unrelated_revision: u32,
    #[input(durability = High)]
    edition: u32,
}

pub(crate) struct Syntax {
    names: Vec<String>,
    locations: Vec<u32>,
}

#[ajika::query(transient)]
pub(crate) fn parse(db: &Db, file: u32) -> ajika::Result<Syntax> {
    let source = File::at(file).source(db)?;
    let mut offset = 0;
    let mut names = Vec::new();
    let mut locations = Vec::new();
    for name in source.text.split_whitespace() {
        offset += source.text[offset..]
            .find(name)
            .expect("token is in source");
        locations.push(u32::try_from(offset).expect("small fixture source"));
        names.push(name.to_owned());
        offset += name.len();
    }
    Ok(Syntax { names, locations })
}

#[ajika::output]
pub(crate) struct Analysis {
    pub(crate) names: Vec<String>,
    pub(crate) locations: Vec<u32>,
}

#[ajika::query]
pub(crate) fn analyze(db: &Db, file: u32) -> ajika::Result<Analysis> {
    let syntax = parse(db, file)?;
    Analysis::new(db, syntax.names.clone(), syntax.locations.clone())
}

#[ajika::query]
pub(crate) fn summary(db: &Db, file: u32) -> ajika::Result<u64> {
    let names = analyze(db, file)?.names.value(db)?;
    let edition = configuration(db)?;
    Ok(names.iter().map(|name| name.len() as u64).sum::<u64>() + u64::from(*edition))
}

#[ajika::query]
pub(crate) fn navigation(db: &Db, file: u32) -> ajika::Result<u64> {
    Ok(analyze(db, file)?
        .locations
        .value(db)?
        .iter()
        .map(|&at| u64::from(at))
        .sum())
}

#[ajika::query]
pub(crate) fn configuration(db: &Db) -> ajika::Result<u32> {
    Project::at(()).edition(db)
}

#[ajika::query]
pub(crate) fn index(db: &Db) -> ajika::Result<u64> {
    let mut total = 0;
    for &file in Project::at(()).files(db)?.iter() {
        total += *summary(db, file)?;
    }
    Ok(total)
}

/// Repeated reads in one frame and distinct frames sharing the same child are different paths.
/// This query covers the former; the index's summaries share configuration through the latter.
#[ajika::query(transient)]
pub(crate) fn repeated_reads(db: &Db, reads: u32) -> ajika::Result<u64> {
    let mut total = 0;
    for _ in 0..reads {
        total += *summary(db, 0)?;
    }
    Ok(total)
}

/// A large sibling makes accidental decoding of all of a producer's outputs visible.
#[ajika::query]
pub(crate) fn payloads(
    db: &Db,
    bytes: u32,
) -> ajika::Result<(ajika::Output<u64>, ajika::Output<Vec<u8>>)> {
    Ok((
        db.output("small", Arc::new(42))?,
        db.output("large", Arc::new(vec![7; bytes as usize]))?,
    ))
}

#[derive(Clone, Copy)]
pub(crate) enum Edit {
    Original,
    Whitespace,
    Meaning,
}

#[derive(Clone, Copy)]
pub(crate) enum View {
    Saved,
    Draft,
}

pub(crate) struct Sources {
    pub(crate) saved: Inputs,
    pub(crate) files: u32,
    // Keep edits ready before timing. Names change in Meaning; only offsets change in Whitespace.
    variants: [Source; 3],
}

impl Sources {
    pub(crate) fn new(files: u32) -> Self {
        let text = (0..16)
            .map(|name| format!("name_{name:02}"))
            .collect::<Vec<_>>()
            .join(" ");
        let variants = [
            text.clone(),
            format!(" {text}"),
            format!("{text} extra_name"),
        ]
        .map(|text| Source {
            text: Arc::new(text),
            lifetime: Lifetime::Persistent,
        });
        let saved = Inputs::default()
            .edit(|update| {
                let project = Project::at(());
                project.set_files(update, Arc::new((0..files).collect()))?;
                project.set_unrelated_revision(update, 0)?;
                project.set_edition(update, 2024)?;
                for file in 0..files {
                    File::at(file).set_source(update, variants[0].clone())?;
                }
                Ok(())
            })
            .expect("fixture inputs");
        Self {
            saved,
            files,
            variants,
        }
    }

    pub(crate) fn edited(&self, edit: Edit, view: View) -> Inputs {
        let (base, lifetime) = match view {
            View::Saved => (self.saved.clone(), Lifetime::Persistent),
            View::Draft => (self.saved.overlay(), Lifetime::Session),
        };
        base.edit(|update| File::at(0).set_source(update, self.source(edit, lifetime)))
            .expect("edited fixture input")
    }

    pub(crate) fn source(&self, edit: Edit, lifetime: Lifetime) -> Source {
        let mut source = self.variants[edit as usize].clone();
        source.lifetime = lifetime;
        source
    }

    pub(crate) fn expected_index(&self, edit: Edit) -> u64 {
        u64::from(self.files) * (16 * 7 + 2024) + if matches!(edit, Edit::Meaning) { 10 } else { 0 }
    }
}
