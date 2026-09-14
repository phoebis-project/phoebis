use phoebis_profile::{claude_code_2_1_269_linux_x64, fixtures};
use phoebis_transport::{connector, hello, Plane};

#[tokio::test]
async fn inference_plane_hello_matches_the_fixture_after_masking() {
    let c = connector(claude_code_2_1_269_linux_x64(), Plane::Inference).unwrap();
    let ours = hello::capture_client_hello(&c, "api.anthropic.com").await;
    let theirs = fixtures::client_hello(fixtures::Plane::Inference);
    assert_eq!(ours.len(), theirs.len(), "record length");
    assert_eq!(hello::mask(&ours), hello::mask(&theirs));
}

#[tokio::test]
async fn control_plane_hello_matches_the_fixture_after_masking() {
    let c = connector(claude_code_2_1_269_linux_x64(), Plane::Control).unwrap();
    let ours = hello::capture_client_hello(&c, "api.anthropic.com").await;
    let theirs = fixtures::client_hello(fixtures::Plane::Control);
    assert_eq!(ours.len(), theirs.len(), "record length");
    assert_eq!(hello::mask(&ours), hello::mask(&theirs));
}
