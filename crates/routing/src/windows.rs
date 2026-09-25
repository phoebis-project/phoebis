//! The window whose reset refunds an account's allowance for a model.

use crate::state::{SEVEN_DAY, SEVEN_DAY_FABLE};

/// A row per model family that carries a window of its own, matched on the folded id; every other model reads
/// the shared week.
pub const MODEL_WINDOWS: &[(&str, &str)] = &[("fable", SEVEN_DAY_FABLE)];

/// The first row whose family the folded id contains, else the shared weekly window.
pub fn window_for(canonical_model: &str) -> &'static str {
    MODEL_WINDOWS
        .iter()
        .find(|(family, _)| canonical_model.contains(family))
        .map(|(_, window)| *window)
        .unwrap_or(SEVEN_DAY)
}
