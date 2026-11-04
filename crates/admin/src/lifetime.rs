//! A lifetime total is the rows older than the week's start, read once per week-start hour and cached per route,
//! plus the week the route already reads per poll. The split serves where a timed cache of the whole total would not:
//! every payload carries a lifetime at or above its week, a row the writer just landed inside the week shows on the
//! next poll, and the walk over the old rows runs once per hour per route rather than on every poll of every page.

use std::collections::BTreeMap;
use std::future::Future;
use std::ops::AddAssign;

use phoebis_ledger::{LedgerError, Totals};

/// The key a route's older part is cached under: the week-start hour the route derives from its clock. The hour also
/// pins the cost revision in force at the poll's clock, whose anchor prices a row's Pro-week figure: a revision takes
/// force at a UTC midnight, an hour boundary, and the tables hold for the process's life, so the revision in force
/// moves only when the week-start hour does and the two parts add under one anchor.
pub type OldKey = i64;

/// The hour a lifetime splits at, seven days before the poll's hour: the week's part reads from it and the older part
/// up to it.
pub fn week_start(hour: i64) -> i64 {
    hour - 7 * 24 * 3_600
}

/// One route's older part of its lifetime totals: the rows older than the week's start, summed per group, recomputed
/// by the first poll that finds the key moved and shared with the polls that arrive while that read runs, which wait
/// on the lock rather than read again. An error caches nothing, so the next poll reads again. A route reads it from
/// `i64::MIN`, which sits at or below the earliest hour the rollups can hold, so every row older than the week clears
/// it.
pub struct OldPart<V>(tokio::sync::Mutex<Option<(OldKey, BTreeMap<String, V>)>>);

impl<V> Default for OldPart<V> {
    fn default() -> Self {
        OldPart(tokio::sync::Mutex::new(None))
    }
}

impl<V: Clone> OldPart<V> {
    pub async fn get<F>(
        &self,
        key: OldKey,
        read: impl FnOnce() -> F,
    ) -> Result<BTreeMap<String, V>, LedgerError>
    where
        F: Future<Output = Result<BTreeMap<String, V>, LedgerError>>,
    {
        let mut cached = self.0.lock().await;
        if let Some((_, part)) = cached.as_ref().filter(|(at, _)| *at == key) {
            return Ok(part.clone());
        }
        let part = read().await?;
        // A poll that read its clock before the hour turned can take the lock after one that read it after, so an
        // older hour answers its own read and leaves the newer entry for the polls that follow.
        if cached.as_ref().is_none_or(|(at, _)| *at <= key) {
            *cached = Some((key, part.clone()));
        }
        Ok(part)
    }
}

/// The older parts of the two polled routes that answer a lifetime total: the accounts route reads its request
/// counts alone, the keys route its priced totals per generation, which it filters to the live ones on each poll.
#[derive(Default)]
pub struct OldTotals {
    pub accounts: OldPart<u64>,
    pub keys: OldPart<BTreeMap<i64, Totals>>,
}

/// `old` and `week` added per group. Every field of a group's figure is a sum, so the older part and the week add to
/// the figure one walk over every row answers.
pub fn lifetime_of<V: Default + for<'a> AddAssign<&'a V>>(
    mut old: BTreeMap<String, V>,
    week: &BTreeMap<String, V>,
) -> BTreeMap<String, V> {
    for (group, figure) in week {
        *old.entry(group.clone()).or_default() += figure;
    }
    old
}
