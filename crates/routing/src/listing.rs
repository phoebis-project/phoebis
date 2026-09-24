//! The shape of an upstream's model listing and the dated-id fold every canonical lookup shares.

/// One row of an upstream's `GET /v1/models` answer, the five fields the listing repeats. No serde derive: the
/// routing crate carries none, and the two routes that answer it build their JSON by hand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogRow {
    pub id: String,
    pub display_name: String,
    pub created_at: String,
    pub max_input_tokens: Option<u64>,
    pub max_tokens: Option<u64>,
}

/// The id with a trailing `-YYYYMMDD` removed, `None` when it carries none: a dated id's undated base, which
/// a price row or a cache TTL override written before the dated id existed is still keyed on.
pub fn undated(id: &str) -> Option<&str> {
    let (base, suffix) = id.rsplit_once('-')?;
    (suffix.len() == 8 && suffix.bytes().all(|b| b.is_ascii_digit())).then_some(base)
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
}
