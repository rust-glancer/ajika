//! A synchronous adapter around the application host. One thread owns input publication
//! and operations; Ajika's scoped workers provide parallelism inside handbook builds.

use std::{
    io::{Cursor, Read},
    sync::{
        Arc,
        mpsc::{self, Receiver},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, notify};
use serde::{Deserialize, Serialize};
use tiny_http::{Header, Method, Request, Response};

use crate::{
    host::{Host, State, Work},
    queries::Page,
};

pub(crate) struct Web {
    host: Host,
    server: tiny_http::Server,
    changes: Receiver<DebounceEventResult>,
    // Dropping the debouncer stops notifications, so retain it for the server's lifetime.
    _watcher: Debouncer<notify::RecommendedWatcher, RecommendedCache>,
    watch_error: Option<String>,
    authority: String,
}

impl Web {
    pub(crate) fn listen(mut host: Host, port: u16) -> Result<Self> {
        let server = tiny_http::Server::http(("127.0.0.1", port))
            .map_err(anyhow::Error::from_boxed)
            .context("listen for handbook previews")?;
        let authority = server.server_addr().to_string();
        let (send, changes) = mpsc::channel();
        let mut watcher =
            notify_debouncer_full::new_debouncer(Duration::from_millis(100), None, send)?;
        watcher.watch(host.root(), notify::RecursiveMode::Recursive)?;
        // Close the startup gap between the first source capture and enabling notifications.
        host.refresh()?;
        println!("Handbook editor: http://{authority}");
        Ok(Self {
            host,
            server,
            changes,
            _watcher: watcher,
            watch_error: None,
            authority,
        })
    }

    pub(crate) fn run(mut self) -> Result<()> {
        loop {
            self.observe_changes();
            let Some(mut request) = self.server.recv_timeout(Duration::from_millis(50))? else {
                continue;
            };
            let response = match self.route(&mut request) {
                Ok(response) => response,
                Err(error) => Self::json(&serde_json::json!({ "error": format!("{error:#}") }))?
                    .with_status_code(400),
            }
            .with_header(Header::from_bytes("Cache-Control", "no-store").expect("static header"))
            .with_header(
                Header::from_bytes("X-Content-Type-Options", "nosniff").expect("static header"),
            );
            // A closed browser connection does not invalidate completed query work.
            if let Err(error) = request.respond(response) {
                eprintln!("respond to browser: {error}");
            }
        }
    }

    fn observe_changes(&mut self) {
        let mut changed = false;
        for batch in self.changes.try_iter() {
            match batch {
                Ok(events) => {
                    // Linux also reports our own source reads. Preserve event kinds through
                    // debouncing so those reads cannot feed back into repeated captures.
                    changed |= events.iter().any(|event| {
                        !event.kind.is_access()
                            && event.paths.iter().any(|path| {
                                path.strip_prefix(self.host.root()).is_ok_and(|relative| {
                                    !relative.components().any(|part| {
                                        part.as_os_str().to_string_lossy().starts_with('.')
                                    })
                                })
                            })
                    });
                }
                Err(errors) => {
                    self.watch_error = Some(
                        errors
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join("\n"),
                    )
                }
            }
        }
        if changed {
            self.watch_error = self.host.refresh().err().map(|error| format!("{error:#}"));
        }
    }

    fn route(&mut self, request: &mut Request) -> Result<Response<Cursor<Vec<u8>>>> {
        // This is a local editor with write access. Reject cross-origin browser commands
        // and arbitrary Host headers; no CORS or public-network mode is provided.
        let header = |name: &str| {
            request
                .headers()
                .iter()
                .find(|header| header.field.as_str().as_str().eq_ignore_ascii_case(name))
                .map(|header| header.value.as_str())
        };
        anyhow::ensure!(
            header("Host") == Some(self.authority.as_str()),
            "use the printed localhost URL"
        );
        let origin = format!("http://{}", self.authority);
        anyhow::ensure!(
            header("Origin").is_none_or(|value| value == origin),
            "different browser origin"
        );
        match (request.method(), request.url()) {
            (&Method::Get, "/") => Ok(Response::from_data(include_bytes!("editor.html").to_vec())
                .with_header(
                    Header::from_bytes("Content-Type", "text/html; charset=utf-8")
                        .expect("static header"),
                )),
            (&Method::Get, "/api/state") => Self::json(&Status {
                state: self.host.state(),
                watch_error: self.watch_error.as_deref(),
            }),
            (&Method::Post, "/api") => {
                anyhow::ensure!(
                    header("Content-Type").is_some_and(|value| {
                        value.split(';').next().is_some_and(|mime| {
                            mime.trim().eq_ignore_ascii_case("application/json")
                        })
                    }),
                    "expected JSON command"
                );
                let mut body = Vec::new();
                request
                    .as_reader()
                    .take(2 * 1024 * 1024 + 1)
                    .read_to_end(&mut body)?;
                anyhow::ensure!(
                    body.len() <= 2 * 1024 * 1024,
                    "document exceeds the example's 2 MiB request limit"
                );
                Self::json(&self.command(serde_json::from_slice(&body)?)?)
            }
            _ => Ok(Response::from_string("Not found").with_status_code(404)),
        }
    }

    fn json(value: &impl Serialize) -> Result<Response<Cursor<Vec<u8>>>> {
        Ok(Response::from_data(serde_json::to_vec(value)?).with_header(
            Header::from_bytes("Content-Type", "application/json").expect("static header"),
        ))
    }

    fn command(&mut self, command: Command) -> Result<Reply> {
        let page = match command {
            Command::Build => {
                let run = self.host.build()?;
                return Ok(Reply {
                    state: self.host.state(),
                    text: None,
                    pages: run.value,
                    work: Some(run.work),
                });
            }
            Command::Open { page } => page,
            Command::Preview { page, text } => {
                self.host.edit(&page, text)?;
                page
            }
            Command::Save { page, text } => {
                self.host.edit(&page, text)?;
                self.host.save()?;
                page
            }
            Command::Discard { page } => {
                self.host.discard()?;
                let state = self.host.state();
                if state.pages.contains(&page) {
                    page
                } else {
                    let Some(first) = state.pages.first().cloned() else {
                        return Ok(Reply {
                            state,
                            text: None,
                            pages: Vec::new(),
                            work: None,
                        });
                    };
                    first
                }
            }
        };
        let text = self.host.text(&page)?;
        let run = self.host.preview(&page)?;
        Ok(Reply {
            state: self.host.state(),
            text: Some(text),
            pages: vec![run.value],
            work: Some(run.work),
        })
    }
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Command {
    Open { page: String },
    Preview { page: String, text: String },
    Save { page: String, text: String },
    Discard { page: String },
    Build,
}

#[derive(Serialize)]
struct Reply {
    state: State,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<Arc<String>>,
    pages: Vec<Page>,
    #[serde(skip_serializing_if = "Option::is_none")]
    work: Option<Work>,
}

#[derive(Serialize)]
struct Status<'a> {
    state: State,
    watch_error: Option<&'a str>,
}
