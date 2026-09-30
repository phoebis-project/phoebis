use phoebis_routing::listing::{find, listing, page, CatalogRow, LONG_CONTEXT_SUFFIX};
use phoebis_routing::rules::process;
use phoebis_store::{ModelRule, RulesMode};

fn row(id: &str, max_input: u64) -> CatalogRow {
    CatalogRow {
        id: id.into(),
        display_name: id.into(),
        created_at: "2025-01-01T00:00:00Z".into(),
        max_input_tokens: Some(max_input),
        max_tokens: Some(8_192),
        effort_levels: Vec::new(),
    }
}

/// "NOT x" builds a negated entry, anything else a plain one: the shorthand these tests use for a pool's list.
fn pool_rules(entries: &[&str]) -> Vec<ModelRule> {
    entries
        .iter()
        .map(|entry| match entry.strip_prefix("NOT ") {
            Some(pattern) => ModelRule {
                pattern: pattern.into(),
                not: true,
            },
            None => ModelRule {
                pattern: (*entry).into(),
                not: false,
            },
        })
        .collect()
}

#[test]
fn the_listing_merges_members_in_priority_order_applies_the_rules_and_adds_the_twins() {
    let sonnet = row("claude-sonnet-4-5-20250929", 1_000_000);
    let haiku = row("claude-haiku-4-5-20251001", 200_000);
    let member1 = vec![sonnet.clone(), haiku.clone()];
    let mut sonnet_renamed = sonnet.clone();
    sonnet_renamed.display_name = "Sonnet, another member's name".into();
    let opus = row("claude-opus-5", 1_000_000);
    let member2 = vec![sonnet_renamed, opus];
    let processed = process(&[], RulesMode::Extend, &pool_rules(&["NOT claude-opus*"]));

    let mut twin = sonnet.clone();
    twin.id = format!("{}{LONG_CONTEXT_SUFFIX}", sonnet.id);
    twin.display_name = format!("{} (1M Context)", sonnet.display_name);

    let with_twins = listing(&[member1.clone(), member2.clone()], &processed, true);
    assert_eq!(
        with_twins,
        vec![haiku.clone(), sonnet.clone(), twin],
        "member 1's sonnet row wins the fold and the twin follows it"
    );

    let without_twins = listing(&[member1, member2], &processed, false);
    assert_eq!(without_twins, vec![haiku, sonnet]);
}

#[test]
fn a_member_with_no_rows_contributes_nothing() {
    let rows = vec![row("claude-sonnet-4-5-20250929", 1_000_000)];
    let processed = process(&[], RulesMode::Extend, &[]);
    assert_eq!(
        listing(&[vec![], rows.clone()], &processed, false),
        listing(&[rows], &processed, false)
    );
}

#[test]
fn the_page_honours_limit_and_the_two_cursors() {
    let rows: Vec<CatalogRow> = (1..=5).map(|n| row(&format!("m{n}"), 1_000)).collect();
    let processed = process(&[], RulesMode::Extend, &[]);
    let listed = listing(&[rows], &processed, false);

    let first_two = page(&listed, 2, None, None);
    assert_eq!(first_two.data, listed[0..2]);
    assert!(first_two.has_more);
    assert_eq!(first_two.first_id.as_deref(), Some("m1"));
    assert_eq!(first_two.last_id.as_deref(), Some("m2"));

    let after_second = page(&listed, 2, Some("m2"), None);
    assert_eq!(after_second.data, listed[2..4]);
    assert!(after_second.has_more);

    let before_last = page(&listed, 2, None, Some("m5"));
    assert_eq!(
        before_last.data,
        listed[2..4],
        "rows precede the cursor, so the page ends just before it"
    );
    assert!(before_last.has_more);

    let all = page(&listed, 1000, None, None);
    assert_eq!(all.data, listed);
    assert!(!all.has_more);
}

#[test]
fn find_matches_an_id_and_a_twin_case_insensitively() {
    let sonnet = row("claude-sonnet-4-5-20250929", 1_000_000);
    let processed = process(&[], RulesMode::Extend, &[]);
    let listed = listing(&[vec![sonnet]], &processed, true);
    assert_eq!(
        find(&listed, "CLAUDE-SONNET-4-5-20250929"),
        Some(&listed[0])
    );
    assert_eq!(
        find(&listed, "claude-sonnet-4-5-20250929[1M]"),
        Some(&listed[1])
    );
    assert_eq!(find(&listed, "nope"), None);
}
