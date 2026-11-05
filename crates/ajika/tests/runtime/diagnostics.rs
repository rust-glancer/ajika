//! Collect structured events rather than testing a subscriber's text layout or timestamps.

use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, Mutex},
};

use tracing::{
    Event, Level, Metadata, Subscriber,
    field::{Field, Visit},
    span,
};

#[derive(Debug, Default)]
pub(super) struct Events(pub(super) Vec<BTreeMap<&'static str, String>>);

impl Events {
    pub(super) fn assert_contains(&self, query: &str, fields: &[(&str, &str)]) {
        assert!(
            self.0.iter().any(|event| {
                event["query"].ends_with(&format!("::{query}"))
                    && fields
                        .iter()
                        .all(|(key, value)| event.get(key).is_some_and(|actual| actual == value))
            }),
            "missing {query} {fields:?} in {self:#?}"
        );
    }

    /// Every event must identify its operation even when it came from another worker.
    pub(super) fn operation(&self, revision: u64) -> u64 {
        let op: u64 = self.0.first().expect("diagnostic events")["op"]
            .parse()
            .expect("operation ID");
        for event in &self.0 {
            assert_eq!(event["op"], op.to_string());
            assert_eq!(event["revision"], revision.to_string());
            assert_eq!(event["call_id"].len(), 64);
            assert_eq!(event["call"], event["call_id"][..8]);
        }
        op
    }
}

pub(super) fn capture<R>(level: Level, work: impl FnOnce() -> R) -> (R, Events) {
    let events = Arc::new(Mutex::new(Events::default()));
    let recorder = Recorder {
        level,
        events: events.clone(),
    };
    let result = tracing::subscriber::with_default(recorder, work);
    let events = std::mem::take(&mut *events.lock().expect("recorded events"));
    (result, events)
}

struct Recorder {
    level: Level,
    events: Arc<Mutex<Events>>,
}

impl Subscriber for Recorder {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.target() == "ajika::diagnostics" && *metadata.level() <= self.level
    }

    fn event(&self, event: &Event<'_>) {
        struct Fields(BTreeMap<&'static str, String>);
        impl Visit for Fields {
            fn record_str(&mut self, field: &Field, value: &str) {
                self.0.insert(field.name(), value.to_owned());
            }
            fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
                self.0.insert(field.name(), format!("{value:?}"));
            }
        }
        let mut fields = Fields(BTreeMap::new());
        event.record(&mut fields);
        self.events
            .lock()
            .expect("recorded events")
            .0
            .push(fields.0);
    }

    // This subscriber only records events on the diagnostic target, which has no spans.
    fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(1)
    }
    fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
    fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
    fn enter(&self, _: &span::Id) {}
    fn exit(&self, _: &span::Id) {}
}

#[test]
fn filtering_diagnostics_keeps_info_quiet_and_trace_opt_in() -> ajika::Result<()> {
    for level in [Level::INFO, Level::DEBUG, Level::TRACE] {
        let mut db = ajika::Database::memory(super::inputs("hello"), (), super::retention());
        let (result, events) = capture(level, || db.run(|db| super::symbols(db, 1)));
        assert_eq!(result?.as_str(), "symbols:1:hello");
        assert_eq!(db.statistics().executed, 2);
        if level == Level::INFO {
            assert!(events.0.is_empty());
        } else {
            events.operation(1);
            events.assert_contains("tree", &[("message", "execute"), ("reason", "no_record")]);
            if level == Level::DEBUG {
                assert!(events.0.iter().all(|event| event["message"] == "execute"));
            } else {
                events.assert_contains(
                    "tree",
                    &[
                        ("message", "output"),
                        ("result", "added"),
                        ("version", "created"),
                    ],
                );
            }
        }
    }
    Ok(())
}

#[ajika::query]
fn keyed(db: &super::Db) -> ajika::Result<Vec<ajika::Output<u32>>> {
    let outputs = ["private-first-key", "private-second-key"]
        .into_iter()
        .map(|key| db.output(("body", key.to_owned()), Arc::new(7)))
        .collect::<ajika::Result<_>>()?;
    Ok(outputs)
}

#[test]
fn keyed_slots_have_opaque_identities_shared_by_emission_and_reads() -> ajika::Result<()> {
    let mut db = ajika::Database::memory(super::inputs(""), (), super::retention());
    let (result, events) = capture(Level::TRACE, || {
        db.run(|db| {
            let outputs = keyed(db)?;
            for output in outputs.iter() {
                for _ in 0..2 {
                    assert_eq!(*output.value(db)?, 7);
                }
            }
            Ok(())
        })
    });
    result?;
    let outputs: Vec<_> = events
        .0
        .iter()
        .filter(|event| event["message"] == "output" && event["output"].starts_with("body#"))
        .collect();
    assert_eq!(outputs.len(), 2);
    assert_ne!(outputs[0]["output_id"], outputs[1]["output_id"]);
    for output in outputs {
        events.assert_contains(
            "keyed",
            &[("message", "reuse"), ("output_id", &output["output_id"])],
        );
    }
    assert!(
        !format!("{events:?}").contains("private-"),
        "keys remain opaque"
    );
    Ok(())
}
