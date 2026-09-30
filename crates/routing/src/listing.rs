//! The shape of an upstream's model listing, the dated-id fold every canonical lookup shares, and the caller-key
//! listing `GET /v1/models` answers from it.

use std::collections::HashSet;

use crate::rules::{allows, ProcessedRule};

/// One row of an upstream's `GET /v1/models` answer: the five fields the listing repeats and the effort levels
/// its capabilities name. No serde derive: the routing crate carries none, and the two routes that answer it
/// build their JSON by hand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogRow {
    pub id: String,
    pub display_name: String,
    pub created_at: String,
    pub max_input_tokens: Option<u64>,
    pub max_tokens: Option<u64>,
    /// The effort levels the listing says the model supports, in the API's order (`low`, `medium`, `high`,
    /// `xhigh`, `max`, then any other name as listed); empty when the listing names none. Shown, never routed on.
    pub effort_levels: Vec<String>,
}

/// The id with a trailing `-YYYYMMDD` removed, `None` when it carries none: a dated id's undated base, which
/// a price row or a cache TTL override written before the dated id existed is still keyed on.
pub fn undated(id: &str) -> Option<&str> {
    let (base, suffix) = id.rsplit_once('-')?;
    (suffix.len() == 8 && suffix.bytes().all(|b| b.is_ascii_digit())).then_some(base)
}

/// The exact row, else the row of `undated(canonical_model)`, else `None`; the caller applies its own fallback.
/// Shared by `RateTable::price` and the ledger's tables, since a request's canonical model is a dated catalog id
/// and the rows ship undated.
pub fn row_for<'a, T>(canonical_model: &str, get: impl Fn(&str) -> Option<&'a T>) -> Option<&'a T> {
    get(canonical_model).or_else(|| undated(canonical_model).and_then(&get))
}

/// The `YYYYMMDD` a dated id ends in, as an integer, so two snapshots of one spelling order by date; `None`
/// when the id carries no eight-digit suffix.
pub fn snapshot_date(id: &str) -> Option<u32> {
    let (_, suffix) = id.rsplit_once('-')?;
    (suffix.len() == 8).then(|| suffix.parse().ok()).flatten()
}

/// Input tokens above which a model gets a `[1m]` twin: Claude Code sizes its window at 200K unless the id ends
/// in the suffix, so the twin is what lets it use the window the catalog reports.
pub const LONG_CONTEXT_ABOVE: u64 = 200_000;
pub const LONG_CONTEXT_SUFFIX: &str = "[1m]";

/// The listing for one key: the members' rows in the order given (priority order; a member whose fetch failed
/// hands in an empty list), each row's folded id judged by `processed`, one row per id from the first member
/// that lists it, the twin beside every long-context row when `twins` is set, sorted by id.
pub fn listing(
    members: &[Vec<CatalogRow>],
    processed: &[ProcessedRule],
    twins: bool,
) -> Vec<CatalogRow> {
    let mut folded_seen = HashSet::new();
    let mut kept: Vec<(String, CatalogRow)> = Vec::new();
    for member in members {
        for row in member {
            // The upstream never lists a twin of its own; a row already carrying the suffix is not this
            // account's model to fold under, so it is dropped rather than double-suffixed.
            if row.id.ends_with(LONG_CONTEXT_SUFFIX) {
                continue;
            }
            let folded = row.id.to_ascii_lowercase();
            if folded_seen.insert(folded.clone()) {
                kept.push((folded, row.clone()));
            }
        }
    }
    let mut out: Vec<CatalogRow> = kept
        .into_iter()
        .filter(|(folded, _)| allows(processed, folded))
        .map(|(_, row)| row)
        .collect();
    if twins {
        let twinned: Vec<CatalogRow> = out
            .iter()
            .filter(|row| {
                row.max_input_tokens
                    .is_some_and(|tokens| tokens > LONG_CONTEXT_ABOVE)
            })
            .map(|row| CatalogRow {
                id: format!("{}{LONG_CONTEXT_SUFFIX}", row.id),
                display_name: format!("{} (1M Context)", row.display_name),
                ..row.clone()
            })
            .collect();
        out.extend(twinned);
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// One page of a sorted listing in the Anthropic shape's terms: `after_id` starts past the named id, `before_id`
/// ends before it and takes the last `limit` rows, `limit` bounds the page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    pub data: Vec<CatalogRow>,
    pub first_id: Option<String>,
    pub last_id: Option<String>,
    pub has_more: bool,
}

pub fn page(
    listing: &[CatalogRow],
    limit: usize,
    after_id: Option<&str>,
    before_id: Option<&str>,
) -> Page {
    let candidates: Vec<CatalogRow> = listing
        .iter()
        .filter(|row| match (after_id, before_id) {
            (Some(after), _) => row.id.as_str() > after,
            (None, Some(before)) => row.id.as_str() < before,
            (None, None) => true,
        })
        .cloned()
        .collect();
    let has_more = candidates.len() > limit;
    let data = if before_id.is_some() {
        // The cursor walks backward: the page ends just before it, so it is the last `limit` rows that
        // precede it rather than the first.
        let start = candidates.len().saturating_sub(limit);
        candidates[start..].to_vec()
    } else {
        candidates[..candidates.len().min(limit)].to_vec()
    };
    Page {
        first_id: data.first().map(|row| row.id.clone()),
        last_id: data.last().map(|row| row.id.clone()),
        has_more,
        data,
    }
}

/// The row whose id equals `id` ASCII-case-insensitively, a twin included.
pub fn find<'a>(listing: &'a [CatalogRow], id: &str) -> Option<&'a CatalogRow> {
    listing.iter().find(|row| row.id.eq_ignore_ascii_case(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undated_strips_an_eight_digit_suffix_and_nothing_else() {
        assert_eq!(
            undated("claude-sonnet-4-5-20250929"),
            Some("claude-sonnet-4-5")
        );
        assert_eq!(
            undated("claude-haiku-4-5-20251001"),
            Some("claude-haiku-4-5")
        );
        assert_eq!(
            undated("claude-x-2025"),
            None,
            "a four-digit suffix is not a date"
        );
        assert_eq!(
            undated("claude-sonnet-4-5"),
            None,
            "no trailing digits at all"
        );
        assert_eq!(undated("no-dash-here"), None);
    }

    #[test]
    fn snapshot_date_reads_the_eight_digit_suffix_as_an_integer() {
        assert_eq!(snapshot_date("claude-sonnet-4-5-20250929"), Some(20250929));
        assert_eq!(snapshot_date("claude-sonnet-4-5-20250601"), Some(20250601));
        assert!(
            snapshot_date("claude-sonnet-4-5-20250929")
                > snapshot_date("claude-sonnet-4-5-20250601")
        );
        assert_eq!(
            snapshot_date("claude-sonnet-4-5"),
            None,
            "no suffix to read"
        );
        assert_eq!(
            snapshot_date("claude-x-2025"),
            None,
            "a four-digit suffix is not a date"
        );
    }

    #[test]
    fn the_generic_lookup_reads_the_exact_row_then_the_undated_one_then_answers_none() {
        let rows: std::collections::HashMap<String, u32> = [
            ("claude-sonnet-4-5".to_string(), 1),
            ("claude-sonnet-4-5-20250929".to_string(), 2),
            ("claude-fable-5-1".to_string(), 3),
        ]
        .into_iter()
        .collect();
        let get = |k: &str| rows.get(k);
        assert_eq!(
            row_for("claude-sonnet-4-5-20250929", get),
            Some(&2),
            "the exact row wins over its undated base"
        );
        assert_eq!(
            row_for("claude-fable-5-1-20260601", get),
            Some(&3),
            "a dated id with no row of its own reads its undated one"
        );
        assert_eq!(row_for("claude-sonnet-4-5", get), Some(&1));
        assert_eq!(
            row_for("claude-haiku-4-5-20251001", get),
            None,
            "neither the dated id nor its base has a row, and the caller's fallback is its own"
        );
        assert_eq!(row_for("claude-opus-5", get), None);
    }
}
