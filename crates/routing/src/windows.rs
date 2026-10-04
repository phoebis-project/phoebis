//! The window whose reset refunds an account's allowance for a model.

use crate::state::{SEVEN_DAY, SEVEN_DAY_FABLE};

/// The one table of model families with a weekly window of their own, matched on the folded id; every other
/// model reads the shared week. One row today: only Fable is known to have its own week. A `seven_day_*` reading
/// with no row here is kept, persisted and shown, cools the account on a claim and feeds no allowance; a row is
/// one line, added once the first live run reads the poll's keys and the 429 claims.
pub const MODEL_WINDOWS: &[(&str, &str)] = &[("fable", SEVEN_DAY_FABLE)];

/// The first row whose family the folded id contains, else the shared weekly window.
pub fn window_for(canonical_model: &str) -> &'static str {
    MODEL_WINDOWS
        .iter()
        .find(|(family, _)| canonical_model.contains(family))
        .map(|(_, window)| *window)
        .unwrap_or(SEVEN_DAY)
}
