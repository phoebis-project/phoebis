//! The cache warming: the recipes, the probes with their schedule, and the idle windows.

pub(crate) mod agent;
pub(crate) mod breakeven;
pub(crate) mod causes;
pub(crate) mod config;
pub(crate) mod gate;
pub(crate) mod guard;
pub(crate) mod idle;
pub(crate) mod observation;
pub(crate) mod probe;
pub(crate) mod recipe;
pub(crate) mod requests;
pub(crate) mod savings;
pub(crate) mod state;
pub(crate) mod usage;
