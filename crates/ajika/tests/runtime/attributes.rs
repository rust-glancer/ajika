#![deny(
    unused_variables,
    unused_mut,
    unfulfilled_lint_expectations,
    non_snake_case
)]

use std::sync::Arc;

use ajika::{Database, Result};
type Db<'a> = ajika::Db<'a, ()>;

#[ajika::query]
#[allow(unused_variables)]
fn allowed(cx: &Db<'_>) -> Result<u32> {
    let unused = 0;
    Ok(1)
}

#[ajika::query]
#[cfg_attr(all(), inline, cfg_attr(all(), allow(unused_mut)))]
fn conditional(_cx: &Db<'_>) -> Result<u32> {
    let mut value = 2;
    Ok(value)
}

#[ajika::query]
fn inner(cx: &Db<'_>) -> Result<u32> {
    //! Documentation and lint policy belong to the function, even when its body moves.
    #![allow(unused_variables)]
    let unused = 0;
    Ok(3)
}

#[ajika::query]
#[expect(unused_variables)]
fn expected(_cx: &Db<'_>) -> Result<u32> {
    let unused = 0;
    Ok(4)
}

struct AttributedQuery(u32);

#[ajika::query]
#[expect(unused_variables)]
impl AttributedQuery {
    //! The impl's documentation and lint policy also cover ordinary methods.
    #![allow(unused_mut, clippy::let_and_return)]

    const VALUE: u32 = 5;

    #[ajika::query(transient)]
    #[expect(unused_variables)]
    #[allow(non_snake_case)]
    fn captured(_cx: &Db<'_>, Offset: u32) -> Result<Self> {
        let unused = 0;
        Ok(Self(Self::VALUE + Offset))
    }

    fn value(&self) -> u32 {
        let unused = 0;
        let mut value = self.0;
        value
    }

    #[ajika::query]
    #[cfg_attr(all(), cfg(any()))]
    fn omitted(_cx: &Db<'_>, value: UnavailableType) -> Result<Self> {
        unreachable!()
    }
}

#[ajika::query]
#[cfg_attr(all(), cfg(any()))]
impl UnavailableType {
    #[ajika::query]
    fn captured(_cx: &Db<'_>) -> Result<Self> {
        unreachable!()
    }
}

#[test]
fn query_attributes_keep_their_rust_scope() -> Result<()> {
    let mut db = Database::memory(ajika::Inputs::default(), (), super::retention());
    let values = db.run(|db| {
        Ok([
            *allowed(db)?,
            *conditional(db)?,
            *inner(db)?,
            *expected(db)?,
            AttributedQuery::captured(db, 0)?.value(),
        ])
    })?;
    assert_eq!(values, [1, 2, 3, 4, 5]);
    Ok(())
}

#[ajika::interned]
#[allow(non_snake_case)]
struct ExternalName {
    Spelling: String,
}

#[test]
fn interned_lints_follow_the_generated_fields_and_accessors() -> Result<()> {
    let interner = ajika::Interner::default();
    let name = ExternalName::new(&interner, "external".into())?;
    assert_eq!(name.Spelling(&interner)?, "external");
    Ok(())
}

// Payloads need persistence and value comparison, but the generated type's Debug/Clone/Eq/Hash are
// operations on handles. This type deliberately does not implement those traits.
#[derive(PartialEq, wincode::SchemaRead, wincode::SchemaWrite)]
struct Payload(u32);

#[ajika::output]
#[derive(Hash)]
#[cfg_attr(all(), expect(non_snake_case))]
struct AttributedOutputs {
    /// This field keeps its slot when its Rust spelling changes.
    #[output(name = "value")]
    Value: Payload,
    #[cfg_attr(all(), cfg(any()))]
    omitted: UnavailableType,
    #[cfg_attr(all(), allow(non_snake_case))]
    #[output(optional)]
    Extra: u32,
    r#type: u32,
}

#[ajika::output]
#[cfg_attr(all(), cfg(any()))]
struct DisabledOutputs {
    value: UnavailableType,
}

#[ajika::query]
fn attributed_outputs(db: &Db<'_>) -> Result<AttributedOutputs> {
    let outputs = if super::Settings::at(()).choose_left(db)? {
        // A handwritten producer and the generated constructor must address the same slots.
        AttributedOutputs {
            Value: db.output("value", Arc::new(Payload(7)))?,
            Extra: None,
            r#type: db.output("type", Arc::new(8))?,
        }
    } else {
        AttributedOutputs::new(db, Payload(7), None, Arc::new(8))?
    };
    Ok(outputs)
}

#[test]
fn output_attributes_preserve_slots_and_traits_apply_to_handles() -> Result<()> {
    let mut db = Database::memory(super::inputs(""), (), super::retention());
    let generated = db.run(attributed_outputs)?;
    assert_eq!(*generated, generated.as_ref().clone());
    assert!(generated.Extra.is_none());
    db.run(|db| {
        assert_eq!(generated.Value.value(db)?.0, 7);
        assert_eq!(*generated.r#type.value(db)?, 8);
        Ok(())
    })?;
    db.replace_inputs(
        super::Sources {
            choose_left: true,
            ..Default::default()
        }
        .capture(),
    );
    assert_eq!(generated, db.run(attributed_outputs)?);
    Ok(())
}
