//! A runnable example: the same host serves an editor, exports pages, and runs a walkthrough.

mod document;
mod host;
mod queries;
mod scenario;
mod web;

#[cfg(test)]
mod tests;

use std::{fs, num::NonZeroUsize, path::PathBuf, thread};

use anyhow::Result;
use clap::Parser;

use crate::host::Host;

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve { project, port } => {
            web::Web::listen(project.open(cli.workers)?, port)?.run()
        }
        Command::Build { project, out } => {
            let run = project.open(cli.workers)?.build()?;
            for page in &run.value {
                let path = out.join(&page.path).with_extension("html");
                fs::create_dir_all(path.parent().expect("page output directory"))?;
                fs::write(&path, page.document())?;
                for diagnostic in page.diagnostics.iter() {
                    println!(
                        "{}:{}: {}",
                        page.path, diagnostic.location.line, diagnostic.message
                    );
                }
            }
            // The structured report is also useful for later, controlled engine comparisons.
            println!("{}", serde_json::to_string_pretty(&run.work)?);
            println!("Wrote {} pages to {}", run.value.len(), out.display());
            let issues: usize = run.value.iter().map(|page| page.diagnostics.len()).sum();
            anyhow::ensure!(issues == 0, "handbook contains {issues} link diagnostics");
            Ok(())
        }
        Command::Demo => scenario::run(cli.workers.get()),
    }
}

#[derive(clap::Parser)]
#[command(name = "handbook", version, about)]
struct Cli {
    /// Maximum query workers, including the calling thread.
    #[arg(long, global = true, default_value_t = thread::available_parallelism().unwrap_or(NonZeroUsize::MIN))]
    workers: NonZeroUsize,
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Start the local Markdown editor and filesystem watcher.
    Serve {
        #[command(flatten)]
        project: ProjectArgs,
        /// Local HTTP port; use 0 to let the OS choose one.
        #[arg(long, default_value_t = 3131)]
        port: u16,
    },
    /// Export HTML and report link diagnostics.
    Build {
        #[command(flatten)]
        project: ProjectArgs,
        /// Directory for exported HTML pages.
        #[arg(long, value_name = "DIRECTORY", default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/handbook"))]
        out: PathBuf,
    },
    /// Run an editing walkthrough on temporary copied documents.
    Demo,
}

#[derive(clap::Args)]
struct ProjectArgs {
    /// Directory containing the handbook's Markdown files.
    #[arg(value_name = "DIRECTORY", default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/content"))]
    source: PathBuf,
    /// Saved-query directory (defaults to DIRECTORY/.handbook-cache).
    #[arg(long, value_name = "DIRECTORY", conflicts_with = "memory")]
    cache: Option<PathBuf>,
    /// Keep query results in memory instead of opening a persistent cache.
    #[arg(long)]
    memory: bool,
}

impl ProjectArgs {
    fn open(self, workers: NonZeroUsize) -> Result<Host> {
        let cache = self
            .cache
            .unwrap_or_else(|| self.source.join(".handbook-cache"));
        Host::open(
            &self.source,
            (!self.memory).then_some(cache.as_path()),
            workers.get(),
        )
    }
}
