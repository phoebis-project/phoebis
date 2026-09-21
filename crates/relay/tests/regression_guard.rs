use axum::http::StatusCode;
use phoebis_profile::fixtures::{body, inference_head, BodyFixture, FixtureKind};
use phoebis_store::AccountKind;
use fake_upstream::{FakeUpstream, Respond, Rule};
use tower::ServiceExt;

mod common;
use common::*;

fn ok() -> Rule {
    Rule { path_prefix: "/v1/messages".into(), respond: Respond::Json { status: 200, headers: vec![], body: br#"{"id":"msg_1","type":"message","content":[],"usage":{"input_tokens":1,"output_tokens":1}}"#.to_vec() } }
}

/// The fixture head with its `Content-Length` set to the
/// edited body, and the body appended: the wire the base commit produced.
fn expected_wire(
    head: &[u8],
    native_len: usize,
    edited_body: &[u8],
) -> Vec<u8> {
    let mut text = String::from_utf8(head.to_vec()).unwrap().replace(
        &format!("Content-Length: {native_len}"),
        &format!("Content-Length: {}", edited_body.len()),
    );
    let mut wire = text.into_bytes();
    wire.extend_from_slice(edited_body);
    wire
}

fn with_uuid(native: &[u8]) -> Vec<u8> {
    String::from_utf8(native.to_vec())
        .unwrap()
        .replacen(
            r#"\"account_uuid\":\"\""#,
            &format!(r#"\"account_uuid\":\"{ACCOUNT_UUID}\""#),
            1,
        )
        .into_bytes()
}

async fn relayed(kind: AccountKind, head: &'static [u8], native: &'static [u8]) -> Vec<u8> {
    let up = FakeUpstream::start(vec![ok()]).await;
    let app = relay_with(kind, &up).await;
    let req = request_from_fixture(head, native, KEY);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let wire = wire_of(&up.requests()[0]);
    up.shutdown().await;
    wire
}

#[tokio::test]
async fn the_first_party_9b244_body_relays_as_the_base_commit_relayed_it() {
    let (head, native) = (
        inference_head(FixtureKind::OauthFirstParty),
        body(BodyFixture::FirstParty9b244),
    );
    let expected_body = phoebis_cch::sign(&with_uuid(native)).unwrap();
    assert_eq!(
        relayed(AccountKind::Oauth, head, native).await,
        expected_wire(head, native.len(), &expected_body)
    );
}

#[tokio::test]
async fn the_first_party_e142e_body_relays_as_the_base_commit_relayed_it() {
    let (head, native) = (
        inference_head(FixtureKind::OauthFirstParty),
        body(BodyFixture::FirstPartyE142e),
    );
    let expected_body = phoebis_cch::sign(&with_uuid(native)).unwrap();
    assert_eq!(
        relayed(AccountKind::Oauth, head, native).await,
        expected_wire(head, native.len(), &expected_body)
    );
}

#[tokio::test]
async fn the_recorded_oauth_count_relays_under_the_defaults_as_the_client_sent_it() {
    let (head, native) = (
        inference_head(FixtureKind::CountOauthNonFirstParty),
        body(BodyFixture::CountOauth),
    );
    assert_eq!(
        relayed(AccountKind::Oauth, head, native).await,
        expected_wire(head, native.len(), native)
    );
}

#[tokio::test]
async fn the_recorded_first_party_count_relays_under_the_defaults_as_the_client_sent_it() {
    let (head, native) = (
        inference_head(FixtureKind::CountOauthFirstParty),
        body(BodyFixture::CountOauthFirstParty),
    );
    assert_eq!(
        relayed(AccountKind::Oauth, head, native).await,
        expected_wire(head, native.len(), native)
    );
}

#[tokio::test]
async fn the_recorded_api_key_count_relays_under_the_defaults_as_the_client_sent_it() {
    let (head, native) = (
        inference_head(FixtureKind::CountApiKey),
        body(BodyFixture::CountApiKey),
    );
    assert_eq!(
        relayed(AccountKind::ApiKey, head, native).await,
        expected_wire(head, native.len(), native)
    );
}
