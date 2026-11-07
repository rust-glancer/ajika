//! Turn the checkpoint's changed values into bounded batches of ready-to-write bytes.
//!
//! Query computation has already finished. These workers only serialize immutable values and
//! compress their bytes; they do not call queries or share a database writer. Once a batch joins,
//! the caller appends it sequentially and releases all its encoded buffers before the next batch.

use std::{
    panic, thread,
    time::{Duration, Instant},
};

use ajika_storage::{Compression, EncodedValue, Write};

use crate::{
    Error, Result,
    runtime::{record::Id, value::Value},
};

pub(crate) struct PendingValue {
    pub(crate) id: Id,
    pub(crate) value: Value,
    pub(crate) length: u64,
}

impl PendingValue {
    /// Bound serialized input per batch. Compression can temporarily hold another copy plus
    /// worker scratch, so this is not a total-memory limit. One oversized value runs alone.
    /// TODO: stream oversized values so they do not need a complete encoded buffer.
    pub(crate) fn write_all(
        pending: &[Self],
        write: &mut Write<'_>,
        compression: Compression,
        workers: usize,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<u64> {
        const BATCH_BYTES: u64 = 16 * 1024 * 1024;
        const BATCH_VALUES: usize = 4096;

        let mut offset = 0;
        let mut bytes_written = 0;
        let mut encoding = Duration::ZERO;
        let mut writing = Duration::ZERO;
        while offset < pending.len() {
            check()?;
            let mut end = offset;
            let mut bytes = 0_u64;
            while let Some(value) = pending.get(end) {
                if end > offset
                    && (bytes.saturating_add(value.length) > BATCH_BYTES
                        || end - offset == BATCH_VALUES)
                {
                    break;
                }
                bytes = bytes.saturating_add(value.length);
                end += 1;
            }
            let batch = &pending[offset..end];
            let started = Instant::now();
            let encode = |values: &[Self]| -> Result<Vec<EncodedValue>> {
                values
                    .iter()
                    .map(|value| {
                        let codec = value.value.codec.expect("persistent value codec");
                        let bytes = (codec.encode)(&value.value.data, value.length)?;
                        compression.encode(bytes).map_err(Error::from)
                    })
                    .collect()
            };
            let encoded = if workers == 1 || batch.len() == 1 {
                encode(batch)?
            } else {
                thread::scope(|scope| -> Result<Vec<EncodedValue>> {
                    let mut chunks = batch.chunks(batch.len().div_ceil(workers.min(batch.len())));
                    let first = chunks.next().expect("nonempty batch");
                    let handles = chunks
                        .map(|chunk| scope.spawn(move || encode(chunk)))
                        .collect::<Vec<_>>();
                    let mut encoded = encode(first)?;
                    for handle in handles {
                        encoded.extend(
                            handle
                                .join()
                                .unwrap_or_else(|panic| panic::resume_unwind(panic))?,
                        );
                    }
                    Ok(encoded)
                })?
            };
            encoding += started.elapsed();
            check()?;
            let started = Instant::now();
            for (value, encoded) in batch.iter().zip(encoded) {
                check()?;
                write.write_value(&value.id, encoded)?;
                bytes_written += value.length;
            }
            writing += started.elapsed();
            offset = end;
        }
        tracing::debug!(
            values = pending.len(),
            bytes_written,
            ?encoding,
            ?writing,
            "checkpoint payloads encoded and written"
        );
        Ok(bytes_written)
    }
}
