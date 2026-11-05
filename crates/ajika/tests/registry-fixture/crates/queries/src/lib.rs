use std::sync::Arc;

use ajika as runtime;
use ajika::{Output, Result};

#[ajika::interned]
pub struct FileId {
    path: String,
}

#[runtime::input(key = u32, crate = runtime)]
pub struct File {
    #[input(arc)]
    text: String,
}

type Db<'a> = ajika::Db<'a, ()>;
type QueryResult<T> = Result<T>;

// This crate has no direct wincode dependency. Derives must use the runtime's re-export,
// and private declaration fields still produce public handles for the consuming crate.
#[runtime::output(crate = runtime)]
#[forbid(non_snake_case)]
pub struct WordOutputs {
    count: u64,
    #[output(optional, name = "source-bytes")]
    bytes: u64,
}

#[runtime::query(crate = runtime)]
impl WordOutputs {
    /// Compute separately reusable counts for this file.
    #[runtime::query]
    pub fn for_file(db: &Db, file: u32) -> QueryResult<Self> {
        let text = File::at(file).text(db)?;
        Self::new(
            db,
            text.split_whitespace().count() as u64,
            Some(Arc::new(text.len() as u64)),
        )
    }

    // Its descriptor must survive linking even though no Rust call names this private function.
    #[runtime::query]
    fn dormant(_db: &Db) -> Result<u64> {
        Ok(42)
    }
}

#[ajika::query]
pub fn summary(db: &Db, file: u32, prefix: String) -> QueryResult<String> {
    Ok(format!(
        "{prefix}: {}",
        WordOutputs::for_file(db, file)?.count.value(db)?
    ))
}

#[ajika::query]
pub fn lengths(db: &Db, file: u32) -> Result<Vec<Output<u64>>> {
    let text = File::at(file).text(db)?;
    let words = text.split_whitespace().enumerate().collect::<Vec<_>>();
    let outputs = db.parallel_map(&words, |db, (index, word)| {
        db.output(("length", *index), Arc::new(word.len() as u64))
    })?;
    Ok(outputs)
}

#[ajika::query]
pub fn parallel_summary(db: &Db, file: u32) -> Result<String> {
    Ok(format!("Words: {}", lengths(db, file)?.len()))
}
