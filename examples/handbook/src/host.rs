//! The application owns files and drafts; Ajika owns the consequences of reading them.
//! Both the server and the scripted walkthrough go through this host.

use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use ajika::{Database, Inputs, Lifetime, Result, Retention};
use anyhow::Context;
use serde::Serialize;

use crate::queries::{Activity, Db, Document, DocumentId, Execution, Page, Source};

pub(crate) struct Host {
    root: PathBuf,
    database: Database<Activity>,
    saved: Inputs,
    files: BTreeMap<String, DocumentId>,
    draft: Option<Draft>,
    generation: u64,
}

impl Host {
    /// A missing cache path selects Ajika's memory backend. Everything above this
    /// constructor uses the same queries and operation boundaries with either backend.
    pub(crate) fn open(root: &Path, cache: Option<&Path>, workers: usize) -> Result<Self> {
        let root = root.canonicalize().context("open handbook directory")?;
        anyhow::ensure!(root.is_dir(), "handbook path is not a directory");
        let retention = Retention {
            max_entries: 100_000,
            max_bytes: 512 * 1024 * 1024,
            max_idle: Duration::from_secs(7 * 24 * 60 * 60),
        };
        let inputs = Inputs::default();
        // Relative IDs belong to this source root. Reusing a cache for another handbook
        // must not accidentally reuse its assignments or observations.
        let epoch = format!("handbook-v2:{}", root.display());
        let mut database = match cache {
            Some(path) => {
                Database::open(path, &epoch, inputs.clone(), Activity::default(), retention)?
            }
            None => Database::memory(inputs.clone(), Activity::default(), retention),
        };
        database.set_parallelism(workers);
        let mut host = Self {
            root,
            database,
            saved: inputs,
            files: BTreeMap::new(),
            draft: None,
            generation: 0,
        };
        // Restore the database's interner before assigning any document IDs.
        host.refresh()?;
        Ok(host)
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn state(&self) -> State {
        let mut pages: Vec<_> = self.files.keys().cloned().collect();
        if let Some(draft) = &self.draft
            && !self.files.contains_key(&draft.path)
        {
            pages.push(draft.path.clone());
            pages.sort();
        }
        State {
            generation: self.generation,
            pages,
            draft: self.draft.as_ref().map(|d| d.path.clone()),
        }
    }

    /// Capture a batch before publishing it. A read failure leaves the previous view installed.
    /// This intentionally scans the small handbook, rather than interpreting editor-specific
    /// rename/write event sequences or making filesystem calls from query providers.
    pub(crate) fn refresh(&mut self) -> Result<bool> {
        let mut captured = BTreeMap::new();
        // Skip hidden cache/output directories and symlinks. Build the whole capture before
        // editing inputs, so an unreadable file cannot publish half of a source update.
        let entries = walkdir::WalkDir::new(&self.root)
            .into_iter()
            .filter_entry(|entry| {
                entry.depth() == 0 || !entry.file_name().to_string_lossy().starts_with('.')
            });
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if !entry.file_type().is_file() || path.extension().is_none_or(|ext| ext != "md") {
                continue;
            }
            let relative = path.strip_prefix(&self.root).expect("scanned descendant");
            let name = relative
                .iter()
                .map(|part| part.to_str().context("handbook paths must be UTF-8"))
                .collect::<Result<Vec<_>>>()?
                .join("/");
            captured.insert(
                name,
                fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?,
            );
        }
        let interner = self.database.interner();
        let mut files = BTreeMap::new();
        let mut replacements = Vec::new();
        for (path, text) in captured {
            let id = DocumentId::new(&interner, path.clone())?;
            if self.saved.get(Document::SOURCE, &id)?.text.as_deref() != Some(&text) {
                replacements.push((id, Some(Arc::new(text))));
            }
            files.insert(path, id);
        }
        for (path, id) in &self.files {
            if !files.contains_key(path) {
                replacements.push((*id, None));
            }
        }
        if replacements.is_empty() && files == self.files {
            return Ok(false);
        }
        let saved = self.saved.edit(|update| {
            // One batch copies the input table once, regardless of the number of changed files.
            for (id, text) in replacements {
                Document::at(id).set_source(
                    update,
                    Source {
                        text,
                        lifetime: Lifetime::Persistent,
                    },
                )?;
            }
            Ok(())
        })?;
        let active = Self::view(&saved, self.draft.as_ref(), &interner)?;
        self.saved = saved;
        self.files = files;
        self.database.replace_inputs(active);
        self.generation += 1;
        Ok(true)
    }

    pub(crate) fn text(&self, path: &str) -> Result<Arc<String>> {
        if let Some(draft) = &self.draft
            && draft.path == path
        {
            return Ok(draft.text.clone());
        }
        let id = self.id(path)?;
        self.saved
            .get(Document::SOURCE, &id)?
            .text
            .context("document no longer exists")
    }

    fn id(&self, path: &str) -> Result<DocumentId> {
        if let Some(id) = self.files.get(path) {
            return Ok(*id);
        }
        if self.draft.as_ref().is_some_and(|d| d.path == path) {
            return DocumentId::new(&self.database.interner(), path.to_owned());
        }
        anyhow::bail!("unknown document: {path}")
    }

    pub(crate) fn edit(&mut self, path: &str, text: String) -> Result<()> {
        anyhow::ensure!(
            self.draft.as_ref().is_none_or(|draft| draft.path == path),
            "save or discard the active draft before opening another document"
        );
        let id = self.id(path)?;
        if *self.text(path)? == text {
            return Ok(());
        }
        let saved = self.saved.get(Document::SOURCE, &id)?.text;
        let draft = if saved.as_deref().is_some_and(|saved| *saved == text) {
            None
        } else {
            let base = self
                .draft
                .as_ref()
                .map(|d| d.base.clone())
                .or(saved)
                .context("document was removed; discard the draft")?;
            Some(Draft {
                path: path.to_owned(),
                text: Arc::new(text),
                base,
            })
        };
        let active = Self::view(&self.saved, draft.as_ref(), &self.database.interner())?;
        self.draft = draft;
        self.database.replace_inputs(active);
        self.generation += 1;
        Ok(())
    }

    fn view(saved: &Inputs, draft: Option<&Draft>, interner: &ajika::Interner) -> Result<Inputs> {
        let Some(draft) = draft else {
            return Ok(saved.clone());
        };
        let id = DocumentId::new(interner, draft.path.clone())?;
        saved.overlay().edit(|update| {
            Document::at(id).set_source(
                update,
                Source {
                    text: Some(draft.text.clone()),
                    lifetime: Lifetime::Session,
                },
            )
        })
    }

    pub(crate) fn save(&mut self) -> Result<()> {
        self.refresh()?;
        let Some(draft) = &self.draft else {
            return Ok(());
        };
        let id = self.id(&draft.path)?;
        anyhow::ensure!(
            self.saved.get(Document::SOURCE, &id)?.text.as_deref() == Some(&*draft.base),
            "the file changed on disk; copy your draft elsewhere or discard it before saving"
        );
        // Saving is a host effect. A memoized query must never be responsible for writing
        // the user's file: its body could be skipped on a cache hit.
        let path = self.root.join(&draft.path);
        let mut saved_file =
            tempfile::NamedTempFile::new_in(path.parent().expect("source parent"))?;
        saved_file
            .as_file()
            .set_permissions(fs::metadata(&path)?.permissions())?;
        saved_file.write_all(draft.text.as_bytes())?;
        saved_file.as_file().sync_all()?;
        // Replace the old file only after the complete new contents have been written.
        saved_file
            .persist(&path)
            .context("replace saved document")?;
        let saved = self.saved.edit(|update| {
            Document::at(id).set_source(
                update,
                Source {
                    text: Some(draft.text.clone()),
                    lifetime: Lifetime::Persistent,
                },
            )
        })?;
        self.saved = saved;
        self.draft = None;
        self.database.replace_inputs(self.saved.clone());
        self.generation += 1;
        // Keep session results until the next query: the identical saved text can promote them.
        Ok(())
    }

    pub(crate) fn discard(&mut self) -> Result<()> {
        self.refresh()?;
        self.draft = None;
        self.database.replace_inputs(self.saved.clone());
        self.database.clear_session();
        self.generation += 1;
        Ok(())
    }

    pub(crate) fn preview(&mut self, path: &str) -> Result<Run<Page>> {
        let id = self.id(path)?;
        self.run(|cx| Page::acquire(cx, id))
    }

    pub(crate) fn build(&mut self) -> Result<Run<Vec<Page>>> {
        let ids = self
            .state()
            .pages
            .iter()
            .map(|path| self.id(path))
            .collect::<Result<Vec<_>>>()?;
        self.run(|cx| cx.parallel_map(&ids, |cx, id| Page::acquire(cx, *id)))
    }

    fn run<T>(&mut self, work: impl FnOnce(&Db<'_>) -> Result<T>) -> Result<Run<T>> {
        self.database.services().take();
        let started = Instant::now();
        let value = self.database.run(work)?;
        let stats = self.database.statistics();
        let work = Work {
            milliseconds: started.elapsed().as_secs_f64() * 1000.0,
            executed: stats.executed,
            validated: stats.validated,
            decoded: stats.decoded,
            bytes_written: stats.bytes_written,
            metadata_bytes_written: stats.metadata_bytes_written,
            session_bytes: stats.session_bytes,
            executions: self.database.services().take(),
        };
        Ok(Run { value, work })
    }
}

struct Draft {
    path: String,
    text: Arc<String>,
    // A watcher may update the saved baseline while this draft remains open.
    // Keep its original text so Save can report a conflict instead of silently overwriting.
    base: Arc<String>,
}

#[derive(Serialize)]
pub(crate) struct State {
    pub(crate) generation: u64,
    pub(crate) pages: Vec<String>,
    pub(crate) draft: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct Run<T> {
    pub(crate) value: T,
    pub(crate) work: Work,
}

#[derive(Serialize)]
pub(crate) struct Work {
    pub(crate) milliseconds: f64,
    pub(crate) executed: u64,
    pub(crate) validated: u64,
    pub(crate) decoded: u64,
    pub(crate) bytes_written: u64,
    pub(crate) metadata_bytes_written: u64,
    pub(crate) session_bytes: u64,
    pub(crate) executions: Vec<Execution>,
}
