use phoebis_store::{
    crypto, AccountKind, AccountRow, AccountState, AttemptOutcome, Identity, NewBundle, Secret,
    StorageKey, Store, StoreError,
};

fn account(id: &str) -> AccountRow {
    AccountRow {
        id: id.into(),
        kind: AccountKind::Oauth,
        label: id.into(),
        enabled: true,
        identity: None,
        lifecycle_generation: 1,
        incarnation: 1,
        revision: 1,
        state: AccountState::Unpaired,
    }
}

fn bundle(token: &str) -> NewBundle {
    NewBundle {
        access_token: Secret::new(token),
        refresh_token: Some(Secret::new("rt-1")),
        issued_at: 1_799_996_400_000,
        expires_at: 1_800_000_000_000,
        refresh_expires_at: None,
        scopes: "user:profile user:inference".into(),
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn credentials_round_trip_encrypted_and_generation_increments() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_account(&account("a")).unwrap();
    let g1 = store
        .commit_credentials("a", 1, 1, None, None, &bundle("at-1"))
        .unwrap();
    let g2 = store
        .commit_credentials("a", 1, 1, Some(g1), None, &bundle("at-2"))
        .unwrap();
    assert!(g2 > g1);
    let got = store.credentials("a").unwrap().unwrap();
    assert_eq!(got.access_token.expose(), "at-2");
    assert_eq!(got.issued_at, 1_799_996_400_000);
    assert_eq!(got.generation, g2);
    for sealed in store.debug_dump_credentials_table().unwrap() {
        assert!(
            !contains(&sealed, b"at-2"),
            "access token bytes found in a sealed blob"
        );
        assert!(
            !contains(&sealed, b"rt-1"),
            "refresh token bytes found in a sealed blob"
        );
    }
}

#[test]
fn sealed_blobs_open_only_with_their_key_row_and_generation() {
    let key = StorageKey::generate();
    let (nonce, sealed) = crypto::seal(&key, "a:1", br#"{"access_token":"at-1"}"#).unwrap();
    assert_eq!(
        crypto::open(&key, "a:1", &nonce, &sealed).unwrap(),
        br#"{"access_token":"at-1"}"#
    );
    assert!(
        crypto::open(&StorageKey::generate(), "a:1", &nonce, &sealed).is_err(),
        "another key"
    );
    let mut tampered = sealed.clone();
    tampered[0] ^= 0x01;
    assert!(
        crypto::open(&key, "a:1", &nonce, &tampered).is_err(),
        "modified ciphertext"
    );
    assert!(
        crypto::open(&key, "b:1", &nonce, &sealed).is_err(),
        "another account row"
    );
    assert!(
        crypto::open(&key, "a:2", &nonce, &sealed).is_err(),
        "another generation"
    );
}

#[test]
fn disconnect_revokes_both_credential_kinds() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_account(&account("a")).unwrap();
    let g = store
        .commit_credentials("a", 1, 1, None, None, &bundle("at-1"))
        .unwrap();
    store
        .set_api_key_secret("a", &Secret::new("sk-ant-api03-x"))
        .unwrap();
    store.open_refresh_attempt("a", 1, 1, g).unwrap();
    let bumped = store.bump_lifecycle("a").unwrap();
    assert!(store.credentials("a").unwrap().is_none());
    assert!(
        store.api_key_secret("a").unwrap().is_none(),
        "disconnect revokes the API key too"
    );
    assert!(
        store.pending_refresh_attempts().unwrap().is_empty(),
        "disconnect resolves the attempt left open from before it"
    );
    assert!(
        store.open_refresh_attempt("a", 1, bumped, 0).is_ok(),
        "the refresh gate is clear again on the new lifecycle generation"
    );
}

#[test]
fn deleting_an_account_ends_its_pending_refresh_attempt() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_account(&account("a")).unwrap();
    let g = store
        .commit_credentials("a", 1, 1, None, None, &bundle("at-1"))
        .unwrap();
    store.open_refresh_attempt("a", 1, 1, g).unwrap();
    store.delete_account("a", "test").unwrap();
    assert!(
        store.pending_refresh_attempts().unwrap().is_empty(),
        "the attempt was the deleted holder's"
    );
    store.insert_account(&account("a")).unwrap();
    let g = store
        .commit_credentials("a", 2, 1, None, None, &bundle("at-2"))
        .unwrap();
    assert!(
        store.open_refresh_attempt("a", 2, 1, g).is_ok(),
        "so the next holder's first refresh is not blocked by it"
    );
}

#[test]
fn an_attempt_carries_the_incarnation_it_began_under() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_account(&account("a")).unwrap();
    let g = store
        .commit_credentials("a", 1, 1, None, None, &bundle("at-1"))
        .unwrap();
    let id = store.open_refresh_attempt("a", 1, 1, g).unwrap();
    store
        .resolve_refresh_attempt(id, 1, AttemptOutcome::Failed("rejected".into()))
        .unwrap();
    store.delete_account("a", "test").unwrap();
    store.insert_account(&account("a")).unwrap();
    assert!(
        store.last_refresh_attempt("a", 2).unwrap().is_none(),
        "the settled attempt is the previous holder's history, not the new one's"
    );
    assert_eq!(
        store
            .last_refresh_attempt("a", 1)
            .unwrap()
            .unwrap()
            .detail
            .as_deref(),
        Some("rejected"),
        "and it stays under the incarnation it began under"
    );
    let g = store
        .commit_credentials("a", 2, 1, None, None, &bundle("at-2"))
        .unwrap();
    store.open_refresh_attempt("a", 2, 1, g).unwrap();
    assert_eq!(
        store
            .pending_refresh_attempts()
            .unwrap()
            .iter()
            .map(|a| a.incarnation)
            .collect::<Vec<_>>(),
        vec![2]
    );
}

#[test]
fn an_uncertain_attempt_blocks_new_attempts() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_account(&account("a")).unwrap();
    let g = store
        .commit_credentials("a", 1, 1, None, None, &bundle("at-1"))
        .unwrap();
    let id = store.open_refresh_attempt("a", 1, 1, g).unwrap();
    store
        .resolve_refresh_attempt(id, 1, AttemptOutcome::Uncertain("response lost".into()))
        .unwrap();
    assert!(matches!(
        store.open_refresh_attempt("a", 1, 1, g),
        Err(StoreError::AttemptPending)
    ));
}

#[test]
fn a_commit_resolves_its_attempt_in_the_same_transaction() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_account(&account("a")).unwrap();
    let g = store
        .commit_credentials("a", 1, 1, None, None, &bundle("at-1"))
        .unwrap();
    let id = store.open_refresh_attempt("a", 1, 1, g).unwrap();
    store
        .commit_credentials("a", 1, 1, Some(g), Some(id), &bundle("at-2"))
        .unwrap();
    assert!(
        store.pending_refresh_attempts().unwrap().is_empty(),
        "a commit leaves no attempt behind"
    );
    assert_eq!(
        store.last_refresh_attempt("a", 1).unwrap().unwrap().outcome,
        Some("committed".to_string()),
        "the commit's own attempt is the one that committed"
    );
}

#[test]
fn a_commit_supersedes_an_attempt_left_open_by_the_bundle_it_replaces() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_account(&account("a")).unwrap();
    let g = store
        .commit_credentials("a", 1, 1, None, None, &bundle("at-1"))
        .unwrap();
    let id = store.open_refresh_attempt("a", 1, 1, g).unwrap();
    store
        .resolve_refresh_attempt(id, 1, AttemptOutcome::Uncertain("response lost".into()))
        .unwrap();
    // A pairing confirmation commits a bundle of its own and has no attempt to name.
    store
        .commit_credentials("a", 1, 1, Some(g), None, &bundle("at-2"))
        .unwrap();
    let attempt = store.last_refresh_attempt("a", 1).unwrap().unwrap();
    assert_eq!(attempt.outcome.as_deref(), Some("failed"));
    assert_eq!(attempt.detail.as_deref(), Some("superseded"));
    assert!(
        store.pending_refresh_attempts().unwrap().is_empty(),
        "the committed bundle answers what the attempt was waiting on"
    );
    assert!(
        store.open_refresh_attempt("a", 1, 1, g + 1).is_ok(),
        "the new bundle can be refreshed"
    );
}

#[test]
fn stale_generation_is_rejected() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_account(&account("a")).unwrap();
    let g1 = store
        .commit_credentials("a", 1, 1, None, None, &bundle("at-1"))
        .unwrap();
    let _g2 = store
        .commit_credentials("a", 1, 1, Some(g1), None, &bundle("at-2"))
        .unwrap();
    assert!(matches!(
        store.commit_credentials("a", 1, 1, Some(g1), None, &bundle("at-3")),
        Err(StoreError::Stale)
    ));
    let bumped = store.bump_lifecycle("a").unwrap();
    assert!(matches!(
        store.commit_credentials("a", 1, 1, None, None, &bundle("at-4")),
        Err(StoreError::Disconnected)
    ));
    assert!(
        store.credentials("a").unwrap().is_none(),
        "disconnect deletes credentials"
    );
    assert_eq!(
        store.account("a").unwrap().unwrap().lifecycle_generation,
        bumped
    );
}

#[test]
fn a_missing_refresh_token_keeps_the_previous_one() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_account(&account("a")).unwrap();
    let g1 = store
        .commit_credentials("a", 1, 1, None, None, &bundle("at-1"))
        .unwrap();
    let mut b = bundle("at-2");
    b.refresh_token = None;
    store
        .commit_credentials("a", 1, 1, Some(g1), None, &b)
        .unwrap();
    assert_eq!(
        store
            .credentials("a")
            .unwrap()
            .unwrap()
            .refresh_token
            .unwrap()
            .expose(),
        "rt-1"
    );
}

#[test]
fn only_one_refresh_attempt_can_be_pending() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_account(&account("a")).unwrap();
    let g = store
        .commit_credentials("a", 1, 1, None, None, &bundle("at-1"))
        .unwrap();
    let id = store.open_refresh_attempt("a", 1, 1, g).unwrap();
    assert!(matches!(
        store.open_refresh_attempt("a", 1, 1, g),
        Err(StoreError::AttemptPending)
    ));
    store
        .resolve_refresh_attempt(id, 1, AttemptOutcome::Uncertain("response lost".into()))
        .unwrap();
    assert_eq!(
        store.pending_refresh_attempts().unwrap().len(),
        1,
        "uncertain attempts stay listed for recovery"
    );
}

#[test]
fn a_bogus_attempt_id_is_rejected_and_leaves_credentials_unchanged() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_account(&account("a")).unwrap();
    let g1 = store
        .commit_credentials("a", 1, 1, None, None, &bundle("at-1"))
        .unwrap();
    assert!(matches!(
        store.commit_credentials("a", 1, 1, Some(g1), Some(999), &bundle("at-2")),
        Err(StoreError::AttemptNotFound(999))
    ));
    let got = store.credentials("a").unwrap().unwrap();
    assert_eq!(got.access_token.expose(), "at-1");
    assert_eq!(got.generation, g1);
}

#[test]
fn secret_debug_never_prints_the_value() {
    assert_eq!(format!("{:?}", Secret::new("hunter2")), "Secret(***)");
}

#[test]
fn an_identity_is_bound_once_and_survives_a_disconnect() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_account(&account("a")).unwrap();
    let bound = Identity {
        account_uuid: "uuid-1".into(),
        email: "one@example.invalid".into(),
        organization_uuid: Some("org-1".into()),
    };
    store.set_identity("a", &bound).unwrap();
    // Re-pairing the same upstream account may refresh the email or the organization it reports.
    store
        .set_identity(
            "a",
            &Identity {
                email: "renamed@example.invalid".into(),
                ..bound.clone()
            },
        )
        .unwrap();
    assert!(matches!(
        store.set_identity(
            "a",
            &Identity {
                account_uuid: "uuid-2".into(),
                ..bound.clone()
            }
        ),
        Err(StoreError::Stale)
    ));
    store
        .commit_credentials("a", 1, 1, None, None, &bundle("at-1"))
        .unwrap();
    store.bump_lifecycle("a").unwrap();
    let after = store.account("a").unwrap().unwrap();
    assert!(store.credentials("a").unwrap().is_none());
    assert_eq!(after.identity.unwrap().account_uuid, "uuid-1");
    assert!(matches!(
        store.set_identity("missing", &bound),
        Err(StoreError::AccountNotFound(_))
    ));
}

#[test]
fn a_configuration_update_rewrites_the_kind_and_label() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_account(&account("a")).unwrap();
    store
        .update_account_config("a", AccountKind::ApiKey, "Renamed")
        .unwrap();
    let row = store.account("a").unwrap().unwrap();
    assert_eq!(row.kind, AccountKind::ApiKey);
    assert_eq!(row.label, "Renamed");
    assert_eq!(row.state, AccountState::Unpaired);
    assert!(matches!(
        store.update_account_config("gone", AccountKind::Oauth, "Gone"),
        Err(StoreError::AccountNotFound(_))
    ));
}

#[test]
fn a_kind_change_is_refused_while_a_credential_is_stored() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_account(&account("a")).unwrap();
    store
        .commit_credentials("a", 1, 1, None, None, &bundle("at-1"))
        .unwrap();
    assert!(matches!(
        store.update_account_config("a", AccountKind::ApiKey, "a"),
        Err(StoreError::KindChange(_))
    ));
    // Only the kind is pinned by the stored bundle; the label still follows the file.
    store
        .update_account_config("a", AccountKind::Oauth, "Renamed")
        .unwrap();
    assert_eq!(store.account("a").unwrap().unwrap().label, "Renamed");

    store.insert_account(&account("b")).unwrap();
    store.set_api_key_secret("b", &Secret::new("sk-1")).unwrap();
    assert!(matches!(
        store.update_account_config("b", AccountKind::ApiKey, "b"),
        Err(StoreError::KindChange(_))
    ));
}

#[test]
fn a_deleted_incarnations_refresh_cannot_commit_into_the_slot_that_took_its_id() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_account(&account("a")).unwrap();
    assert_eq!(store.account("a").unwrap().unwrap().incarnation, 1);
    store
        .commit_credentials("a", 1, 1, None, None, &bundle("at-1"))
        .unwrap();
    store.delete_account("a", "test").unwrap();
    store.insert_account(&account("a")).unwrap();
    assert_eq!(
        store.account("a").unwrap().unwrap().incarnation,
        2,
        "the id was used once before, so what stands there now is its second use"
    );
    store
        .commit_credentials("a", 2, 1, None, None, &bundle("fresh"))
        .unwrap();
    // Every generation restarted at 1 with the new row, so only the incarnation can tell these apart.
    assert!(matches!(
        store.commit_credentials("a", 1, 1, Some(1), None, &bundle("from-the-dead")),
        Err(StoreError::Disconnected)
    ));
    assert_eq!(
        store
            .credentials("a")
            .unwrap()
            .unwrap()
            .access_token
            .expose(),
        "fresh",
        "the replacement keeps the credentials it was paired with"
    );
}

#[test]
fn an_attempt_of_a_deleted_incarnation_settles_nothing_on_the_slot_that_took_its_id() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_account(&account("a")).unwrap();
    store
        .commit_credentials("a", 1, 1, None, None, &bundle("at-1"))
        .unwrap();
    let stranded = store.open_refresh_attempt("a", 1, 1, 1).unwrap();
    store.delete_account("a", "test").unwrap();
    store.insert_account(&account("a")).unwrap();
    assert!(
        matches!(
            store.resolve_refresh_attempt(stranded, 1, AttemptOutcome::Uncertain("lost".into())),
            Err(StoreError::AttemptNotFound(_))
        ),
        "the open attempt went with the holder it began under"
    );
    assert!(
        store.last_refresh_attempt("a", 2).unwrap().is_none(),
        "so nothing the deleted slot left behind quarantines the one that took its id"
    );
}

#[test]
fn resolved_refresh_attempts_are_listed_after_a_watermark() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_account(&AccountRow {
            id: "a".into(),
            kind: AccountKind::Oauth,
            label: "a".into(),
            enabled: true,
            identity: None,
            lifecycle_generation: 1,
            incarnation: 1,
            revision: 1,
            state: AccountState::Ready,
        })
        .unwrap();
    let first = store.open_refresh_attempt("a", 1, 1, 1).unwrap();
    store
        .resolve_refresh_attempt(first, 1, AttemptOutcome::Failed("transport".into()))
        .unwrap();
    let second = store.open_refresh_attempt("a", 1, 1, 1).unwrap();
    assert!(
        store
            .refresh_attempts_resolved_after(0, 0, 10)
            .unwrap()
            .iter()
            .all(|r| r.id == first),
        "an open attempt is not resolved"
    );
    store
        .resolve_refresh_attempt(second, 1, AttemptOutcome::Committed)
        .unwrap();
    let rows = store.refresh_attempts_resolved_after(0, 0, 10).unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| (r.id, r.outcome.as_str(), r.detail.as_deref()))
            .collect::<Vec<_>>(),
        [
            (first, "failed", Some("transport")),
            (second, "committed", None)
        ]
    );
    assert_eq!(
        store
            .refresh_attempts_resolved_after(rows[1].resolved_at, rows[1].id, 10)
            .unwrap()
            .len(),
        0
    );
    // Rows that share a millisecond: paging by (resolved_at, id) crosses the batch boundary without a skip.
    let stamp = rows[1].resolved_at;
    let conn_rows: Vec<u64> = (0..5)
        .map(|i| {
            let id = store.open_refresh_attempt("a", 1, 1, 1).unwrap();
            store
                .resolve_refresh_attempt(id, 1, AttemptOutcome::Failed(format!("tie {i}")))
                .unwrap();
            id
        })
        .collect();
    store.debug_set_resolved_at(&conn_rows, stamp).unwrap();
    let page = store
        .refresh_attempts_resolved_after(rows[0].resolved_at, rows[0].id, 3)
        .unwrap();
    assert_eq!(page.len(), 3);
    let rest = store
        .refresh_attempts_resolved_after(page[2].resolved_at, page[2].id, 10)
        .unwrap();
    assert_eq!(
        page.iter()
            .chain(rest.iter())
            .map(|r| r.id)
            .collect::<Vec<_>>(),
        std::iter::once(second).chain(conn_rows).collect::<Vec<_>>(),
        "every row once, in order"
    );
}

#[test]
fn the_newest_settled_attempt_is_the_mirrors_starting_watermark() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_account(&account("a")).unwrap();
    assert_eq!(store.newest_resolved_refresh_attempt().unwrap(), None);
    let first = store.open_refresh_attempt("a", 1, 1, 1).unwrap();
    store
        .resolve_refresh_attempt(first, 1, AttemptOutcome::Failed("transport".into()))
        .unwrap();
    let open = store.open_refresh_attempt("a", 1, 1, 1).unwrap();
    assert_eq!(
        store
            .newest_resolved_refresh_attempt()
            .unwrap()
            .map(|w| w.1),
        Some(first),
        "an attempt still open is not the newest settled one"
    );
    store
        .resolve_refresh_attempt(open, 1, AttemptOutcome::Committed)
        .unwrap();
    let rows = store.refresh_attempts_resolved_after(0, 0, 10).unwrap();
    assert_eq!(
        store.newest_resolved_refresh_attempt().unwrap(),
        Some((rows[1].resolved_at, rows[1].id)),
        "the watermark is the last row the cursor query would hand out"
    );
    assert_eq!(
        store
            .refresh_attempts_resolved_after(rows[1].resolved_at, rows[1].id, 10)
            .unwrap()
            .len(),
        0,
        "so a mirror starting there replays nothing"
    );
}

#[test]
fn the_resolved_cursor_query_reads_through_its_index() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    // Opened and dropped so the migrations run and the process lock is free for the plain reader below.
    drop(Store::open(&path, &StorageKey::generate()).unwrap());
    let conn = rusqlite::Connection::open(&path).unwrap();
    let mut stmt = conn
        .prepare(
            "EXPLAIN QUERY PLAN SELECT id, account_id, started_at, resolved_at, outcome, detail \
             FROM refresh_attempts WHERE (resolved_at > ?1 OR (resolved_at = ?1 AND id > ?2)) \
             AND outcome IS NOT NULL ORDER BY resolved_at, id LIMIT ?3",
        )
        .unwrap();
    let plan: String = stmt
        .query_map([0i64, 0i64, 500i64], |r| r.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join("\n");
    assert!(
        plan.contains("refresh_attempts_resolved"),
        "the 5 s poll must not scan the table: {plan}"
    );
    assert!(
        !plan.contains("TEMP B-TREE"),
        "and the index supplies the order rather than a sort: {plan}"
    );
}
