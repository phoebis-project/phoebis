use fake_upstream::FakeUpstream;
use phoebis_profile::claude_code_2_1_310_linux_x64;
use phoebis_transport::control::{ResolveFuture, Resolver};
use phoebis_transport::pool::{Pool, PoolKey};
use phoebis_transport::{connector_builder, trust_extra_ca, Plane};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

const HOST: &str = "api.anthropic.com";

fn resolving_to(addr: SocketAddr) -> Resolver {
    Arc::new(move |_: &str| Box::pin(async move { Some(addr) }) as ResolveFuture)
}

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
    let resolver = resolving_to(up.addr());

    let pool = Pool::new(connector(), 4, Duration::from_secs(60));
    let (conn, reused) = pool.checkout(&key, &resolver).await.unwrap();
    assert!(!reused, "the first checkout has to open a connection");
    pool.checkin(key.clone(), conn);
    let (conn, reused) = pool.checkout(&key, &resolver).await.unwrap();
    assert!(reused, "an idle connection comes back marked as reused");
    assert_eq!(up.connections(), 1, "both checkouts shared one connection");
    drop(conn);

    // An entry past its TTL is dropped rather than served, so the caller must be told it got a new connection
    // and not one that has been sitting idle.
    let expiring = Pool::new(connector(), 4, Duration::from_millis(0));
    let (conn, _) = expiring.checkout(&key, &resolver).await.unwrap();
    expiring.checkin(key.clone(), conn);
    let (conn, reused) = expiring.checkout(&key, &resolver).await.unwrap();
    assert!(!reused, "an expired entry is not a reused connection");
    assert_eq!(
        up.connections(),
        3,
        "the expired entry forced a third connection"
    );
    drop(conn);

    up.shutdown().await;
}

#[tokio::test]
async fn an_idle_connection_serves_a_host_that_stopped_resolving() {
    let up = FakeUpstream::start(vec![]).await;
    let profile = claude_code_2_1_310_linux_x64();
    let mut builder = connector_builder(profile, Plane::Inference).unwrap();
    trust_extra_ca(&mut builder, up.ca_pem()).unwrap();
    let key = PoolKey {
        account_id: "acct".into(),
        host: HOST.into(),
    };
    let no_address: Resolver = Arc::new(|_: &str| Box::pin(async { None }) as ResolveFuture);

    let pool = Pool::new(builder.build(), 4, Duration::from_secs(60));
    let (conn, _) = pool.checkout(&key, &resolving_to(up.addr())).await.unwrap();
    pool.checkin(key.clone(), conn);
    let (conn, reused) = pool.checkout(&key, &no_address).await.unwrap();
    assert!(reused, "the idle connection served the host");
    drop(conn);

    up.shutdown().await;
}
