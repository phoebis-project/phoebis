//! The window whose reset refunds an account's allowance for a model.

pub use phoebis_oauth::account_window_id;
pub use phoebis_oauth::SEVEN_DAY_OVERAGE_INCLUDED;

use crate::select::FIVE_HOUR_MS;
use crate::state::{FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE, WEEK_MS};

/// The one table of model families with a weekly window of their own, matched on the folded id; every other
/// model reads the shared week. One row today: only Fable is known to have its own week. A `seven_day_*` reading
/// with no row here is kept, persisted and shown, cools the account on a claim and feeds no allowance; a row is
/// one line, added once the first live run reads the poll's keys and the 429 claims.
pub const MODEL_WINDOWS: &[(&str, &str)] = &[("fable", SEVEN_DAY_FABLE)];

/// The week of its own of the first row whose family the folded id contains, `None` for a model that draws the shared
/// week alone.
pub fn model_week(canonical_model: &str) -> Option<&'static str> {
    MODEL_WINDOWS
        .iter()
        .find(|(family, _)| canonical_model.contains(family))
        .map(|(_, window)| *window)
}

/// The model's own week, else the shared weekly window.
pub fn window_for(canonical_model: &str) -> &'static str {
    model_week(canonical_model).unwrap_or(SEVEN_DAY)
}

/// How long `window` runs: five hours for the 5-hour window and a week for any other name, so a wall's cycle in
/// routing and the ledger's round lookup read one length for every window a stored sample can name.
pub fn window_length_ms(window: &str) -> i64 {
    if window == FIVE_HOUR {
        FIVE_HOUR_MS
    } else {
        WEEK_MS
    }
}
