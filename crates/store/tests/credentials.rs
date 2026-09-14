use phoebis_store::{
    crypto, AccountKind, AccountRow, AccountState, AttemptOutcome, NewBundle, Secret, StorageKey,
    Store, StoreError,
};

fn account(id: &str) -> AccountRow {
    AccountRow {
        id: id.into(),
        kind: AccountKind::Oauth,
        label: id.into(),
        enabled: true,
        identity: None,
        lifecycle_generation: 1,
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
        .commit_credentials("a", 1, None, None, &bundle("at-1"))
        .unwrap();
    let g2 = store
        .commit_credentials("a", 1, Some(g1), None, &bundle("at-2"))
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
        .commit_credentials("a", 1, None, None, &bundle("at-1"))
        .unwrap();
    store
        .set_api_key_secret("a", &Secret::new("sk-ant-api03-x"))
        .unwrap();
    store.open_refresh_attempt("a", 1, g).unwrap();
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
        store.open_refresh_attempt("a", bumped, 0).is_ok(),
        "the refresh gate is clear again on the new lifecycle generation"
    );
}

#[test]
fn an_uncertain_attempt_blocks_new_attempts() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_account(&account("a")).unwrap();
    let g = store
        .commit_credentials("a", 1, None, None, &bundle("at-1"))
        .unwrap();
    let id = store.open_refresh_attempt("a", 1, g).unwrap();
    store
        .resolve_refresh_attempt(id, AttemptOutcome::Uncertain)
        .unwrap();
    assert!(matches!(
        store.open_refresh_attempt("a", 1, g),
        Err(StoreError::AttemptPending)
    ));
}

#[test]
fn a_commit_resolves_its_attempt_in_the_same_transaction() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_account(&account("a")).unwrap();
    let g = store
        .commit_credentials("a", 1, None, None, &bundle("at-1"))
        .unwrap();
    let id = store.open_refresh_attempt("a", 1, g).unwrap();
    store
        .commit_credentials("a", 1, Some(g), Some(id), &bundle("at-2"))
        .unwrap();
    assert!(
        store.pending_refresh_attempts().unwrap().is_empty(),
        "a commit leaves no attempt behind"
    );
}

#[test]
fn stale_generation_is_rejected() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_account(&account("a")).unwrap();
    let g1 = store
        .commit_credentials("a", 1, None, None, &bundle("at-1"))
        .unwrap();
    let _g2 = store
        .commit_credentials("a", 1, Some(g1), None, &bundle("at-2"))
        .unwrap();
    assert!(matches!(
        store.commit_credentials("a", 1, Some(g1), None, &bundle("at-3")),
        Err(StoreError::Stale)
    ));
    let bumped = store.bump_lifecycle("a").unwrap();
    assert!(matches!(
        store.commit_credentials("a", 1, None, None, &bundle("at-4")),
        Err(StoreError::Stale)
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
        .commit_credentials("a", 1, None, None, &bundle("at-1"))
        .unwrap();
    let mut b = bundle("at-2");
    b.refresh_token = None;
    store
        .commit_credentials("a", 1, Some(g1), None, &b)
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
        .commit_credentials("a", 1, None, None, &bundle("at-1"))
        .unwrap();
    let id = store.open_refresh_attempt("a", 1, g).unwrap();
    assert!(matches!(
        store.open_refresh_attempt("a", 1, g),
        Err(StoreError::AttemptPending)
    ));
    store
        .resolve_refresh_attempt(id, AttemptOutcome::Uncertain)
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
        .commit_credentials("a", 1, None, None, &bundle("at-1"))
        .unwrap();
    assert!(matches!(
        store.commit_credentials("a", 1, Some(g1), Some(999), &bundle("at-2")),
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
