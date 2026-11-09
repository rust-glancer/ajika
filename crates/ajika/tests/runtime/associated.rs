use std::sync::Arc;

use ajika::Database;
use anyhow::Result;

use super::{Db, Number};

#[derive(Debug, Clone, PartialEq, Eq, Hash, wincode::SchemaRead, wincode::SchemaWrite)]
struct Scaled(u32);

#[ajika::query]
impl Scaled {
    const FACTOR: u32 = 2;

    #[ajika::query(validate = Self::validate)]
    fn captured(db: &Db<'_>, mut offset: u32) -> ajika::Result<Self> {
        offset += Number::at(true).value(db)?;
        Ok(Self::from_value(offset))
    }

    #[ajika::query(forward, returns(ref), validate = Self::validate)]
    fn forwarded(db: &Db<'_>, offset: u32) -> ajika::Result<Arc<Self>> {
        Self::captured(db, offset)
    }

    #[ajika::query(transient)]
    fn from_previous(_db: &Db<'_>, previous: Self, extra: u32) -> ajika::Result<Self> {
        Ok(Self(previous.value() + extra))
    }

    fn from_value(value: u32) -> Self {
        Self(value * Self::FACTOR)
    }

    fn value(&self) -> u32 {
        self.0
    }

    fn validate(value: &Self) -> ajika::Result<()> {
        if !value.0.is_multiple_of(Self::FACTOR) {
            return Err(ajika::Error::contract("scaled value"));
        }
        Ok(())
    }
}

struct Unscaled;

#[ajika::query]
impl Unscaled {
    #[ajika::query]
    fn captured(db: &Db<'_>, offset: u32) -> ajika::Result<Scaled> {
        Ok(Scaled(offset + Number::at(true).value(db)?))
    }
}

#[test]
fn associated_queries_share_results_and_keep_owners_distinct() -> Result<()> {
    let mut db = Database::memory(super::inputs(""), (), super::retention());
    db.run(|db| {
        let scaled = Scaled::captured(db, 3)?;
        assert_eq!(scaled.value(), 6);
        assert!(Arc::ptr_eq(&scaled, &Scaled::captured(db, 3)?));
        let forwarded: for<'db> fn(&Db<'db>, u32) -> ajika::Result<&'db Scaled> = Scaled::forwarded;
        assert!(std::ptr::eq(scaled.as_ref(), forwarded(db, 3)?));
        assert_eq!(Unscaled::captured(db, 3)?.value(), 3);
        assert_eq!(Scaled::from_previous(db, Scaled(6), 1)?.value(), 7);
        Ok(())
    })?;
    assert_eq!(db.statistics().executed, 4);
    db.update_inputs(|update| Number::at(true).set_value(update, 5))?;
    assert_eq!(db.run(|db| Scaled::captured(db, 3))?.value(), 16);
    assert_eq!(db.statistics().executed, 1);
    Ok(())
}
