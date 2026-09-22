use phoebis_store::{PricingVersion, StorageKey, Store};

#[test]
fn pricing_versions_are_stored_and_the_latest_import_wins() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    assert_eq!(store.schema_version().unwrap(), 18);
    assert!(store.latest_pricing_version().unwrap().is_none());
    let first = PricingVersion { version: "imported-1000".into(), effective: "2026-09-10".into(), source: "https://example.test/a.json".into(), table_json: r#"{"claude-sonnet-5":{"input":2.0,"cache_write_5m":2.5,"cache_write_1h":4.0,"cache_read":0.2,"output":10.0}}"#.into(), imported_at: 1_000 };
    let second = PricingVersion {
        version: "imported-2000".into(),
        imported_at: 2_000,
        ..first.clone()
    };
    store.insert_pricing_version(&first, "admin").unwrap();
    store.insert_pricing_version(&second, "admin").unwrap();
    assert_eq!(store.latest_pricing_version().unwrap().unwrap(), second);
    assert_eq!(store.pricing_versions().unwrap(), vec![first, second]);
    assert!(store
        .registry_changes_after(0, 10)
        .unwrap()
        .iter()
        .any(|c| c.kind == "pricing_imported" && c.target == "imported-2000"));
}
