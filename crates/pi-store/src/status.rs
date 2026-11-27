//! The parts the status line is made of.
//!
//! The names are a config file's: `status` lists these.
//! What each part reads as is the surface's, and is written where it draws.

use serde::{Deserialize, Deserializer, Serialize};

use crate::statusline::Field;

/// Which parts the status line shows, while a run works. A part with nothing
/// to say drops out; the numbers of the run that last ended stay readable
/// from the statusline, which is where they outlive it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Parts(pub Vec<Field>);

// Only the names a status line can draw: a layout-only field — `tabs`, `flash`
// — would be accepted here and then silently dropped at draw time.
impl<'de> Deserialize<'de> for Parts {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let fields = Vec::<Field>::deserialize(d)?;
        match fields
            .iter()
            .find(|f| !f.is_part() && !matches!(f, Field::Model))
        {
            Some(bad) => Err(serde::de::Error::custom(format!(
                "`{}` is not a status line part",
                bad.name()
            ))),
            None => Ok(Self(fields)),
        }
    }
}

impl Default for Parts {
    fn default() -> Self {
        Self(default_parts())
    }
}

impl std::ops::Deref for Parts {
    type Target = [Field];
    fn deref(&self) -> &[Field] {
        &self.0
    }
}

/// The parts when the config names none.
pub fn default_parts() -> Vec<Field> {
    vec![
        Field::Elapsed,
        Field::InOut,
        Field::Speed,
        Field::Cache,
        Field::Ctx,
        Field::Cost,
        // Parts that come and go last, so the ones that stay hold their place.
        Field::Compacted,
        Field::Queued,
    ]
}
