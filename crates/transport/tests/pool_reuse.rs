use fake_upstream::FakeUpstream;
use phoebis_profile::claude_code_2_1_310_linux_x64;
use phoebis_transport::pool::{Pool, PoolKey};
use phoebis_transport::{connector_builder, trust_extra_ca, Plane};
use std::time::Duration;

const HOST: &str = "api.anthropic.com";

#[tokio::test]
async fn checkout_reports_whether_the_connection_came_from_the_idle_list() {
    let up = FakeUpstream::start(vec![]).await;
    let connector = || {
        let profile = claude_code_2_1_310_linux_x64();
        let mut builder = connector_builder(profile, Plane::Inference).unwrap();
        trust_extra_ca(&mut builder, up.ca_pem()).unwrap();
        builder.build()
    };
    let key = PoolKey {
        account_id: "acct".into(),
        host: HOST.into(),
    };

    let pool = Pool::new(connector(), 4, Duration::from_secs(60));
    let (conn, reused) = pool.checkout(key.clone(), HOST, up.addr()).await.unwrap();
    assert!(!reused, "the first checkout has to open a connection");
    pool.checkin(key.clone(), conn);
    let (conn, reused) = pool.checkout(key.clone(), HOST, up.addr()).await.unwrap();
    assert!(reused, "an idle connection comes back marked as reused");
    assert_eq!(up.connections(), 1, "both checkouts shared one connection");
    drop(conn);

    // An entry past its TTL is dropped rather than served, so the caller must be told it got a new connection
    // and not one that has been sitting idle.
    let expiring = Pool::new(connector(), 4, Duration::from_millis(0));
    let (conn, _) = expiring
        .checkout(key.clone(), HOST, up.addr())
        .await
        .unwrap();
    expiring.checkin(key.clone(), conn);
    let (conn, reused) = expiring
        .checkout(key.clone(), HOST, up.addr())
        .await
        .unwrap();
    assert!(!reused, "an expired entry is not a reused connection");
    assert_eq!(
        up.connections(),
        3,
        "the expired entry forced a third connection"
    );
    drop(conn);

    up.shutdown().await;
}
