//! A response head writes a sample in its bucket; a poll in the same bucket does not overwrite it; the Fable
//! window is read from limits[]; extra-usage credits come from the poll only; a paused account is polled; every
//! polled account is read once an interval on its own phase and backs off alone on a 429.
mod common;
use std::cell::Cell;
use std::future::Future;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule};
use http_body_util::BodyExt;
use phoebis_ledger::{
    hourly_window_samples, latest_quota, lead_over_pace, HourlyWindowSample, LedgerConfig,
    LedgerDb, PlanTables, QuotaPoll, QuotaSampleRecord, SampleSource, Sampled, POLL_INTERVAL,
};
use phoebis_oauth::{ClaimOutcome, PollOutcome};
use phoebis_routing::{
    EvidenceSource, Report, UtilizationSample, WindowReading, WindowSource, FIVE_HOUR, SEVEN_DAY,
    SEVEN_DAY_FABLE,
};
use phoebis_store::{
    AccountKind, AccountRow, AccountState, Identity, NewBundle, Secret, StateCause, StorageKey,
    Store, SubscriptionRow,
};
use tokio::time::Instant;
use tower::ServiceExt;

/// The common module's `C` is an API-key secret; an OAuth `c` needs one of the OAuth fixture shape.
const OAUTH_C: &str = "sk-ant-oat01-CCCC";
const OAUTH_D: &str = "sk-ant-oat01-DDDD";
/// The token an account is paired with again after a delete.
const REPAIRED: &str = "sk-ant-oat01-EEEE";

const USAGE: &[u8] = br#"{"five_hour":{"utilization":12.5,"resets_at":"2026-09-13T15:00:00+00:00"},"seven_day":{"utilization":40.0,"resets_at":"2026-09-18T00:00:00+00:00"},"cinder_cove":{"utilization":99.0,"resets_at":null},"limits":[{"kind":"weekly_scoped","group":"weekly","percent":42,"resets_at":"2026-09-18T00:00:00+00:00","scope":{"model":{"display_name":"Fable"}}}],"extra_usage":{"is_enabled":true,"monthly_limit":5000,"used_credits":1234,"utilization":0.2468,"currency":"USD"}}"#;

/// `USAGE` with the Fable week at its limit.
const USAGE_AT_THE_FABLE_LIMIT: &[u8] = br#"{"five_hour":{"utilization":12.5,"resets_at":"2026-09-13T15:00:00+00:00"},"seven_day":{"utilization":40.0,"resets_at":"2026-09-18T00:00:00+00:00"},"cinder_cove":{"utilization":99.0,"resets_at":null},"limits":[{"kind":"weekly_scoped","group":"weekly","percent":100,"resets_at":"2026-09-18T00:00:00+00:00","scope":{"model":{"display_name":"Fable"}}}],"extra_usage":{"is_enabled":true,"monthly_limit":5000,"used_credits":1234,"utilization":0.2468,"currency":"USD"}}"#;

/// A usage body in the client's shape: the limit reset block with one grant, and the spend members null.
const BLOCK: &[u8] = br#"{"five_hour":{"utilization":12.5,"resets_at":null},"extra_usage":null,"spend":null,"cedar_ember":{"eligible":true,"grants":[{"id":"rg-a-1","label":"Launch reset","clears":["seven_day_overage_included"],"gives_back_weekly":true,"starts_at":"2026-01-01T00:00:00Z","ends_at":"2027-01-01T00:00:00Z","resets_left":1,"resets_total":1,"usable_now":true}]}}"#;

/// The target of the limit resets read, the client's shape of the usage read.
const LIMIT_RESETS_TARGET: &str = "/api/oauth/usage?cedar_ember=1&skip_spend=1";

fn usage_rule() -> Rule {
    Rule {
        path_prefix: "/api/oauth/usage".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: USAGE.to_vec(),
        },
    }
}

/// A rule for the client's shape, listed before the usage rule, so the fake fills the block for that shape alone, as
/// the upstream does.
fn limit_resets_rule() -> Rule {
    Rule {
        path_prefix: LIMIT_RESETS_TARGET.into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: BLOCK.to_vec(),
        },
    }
}

/// `usage_rule` with the Fable week at its limit.
fn fable_limit_usage_rule() -> Rule {
    Rule {
        path_prefix: "/api/oauth/usage".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: USAGE_AT_THE_FABLE_LIMIT.to_vec(),
        },
    }
}

fn profile_rule() -> Rule {
    Rule {
        path_prefix: "/api/oauth/profile".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: phoebis_profile::fixtures::control_response(
                phoebis_profile::fixtures::ControlFixture::Profile,
            )
            .to_vec(),
        },
    }
}

fn usage_gated() -> Rule {
    Rule {
        path_prefix: "/api/oauth/usage".into(),
        respond: Respond::Gated(Box::new(usage_rule().respond)),
    }
}

fn profile_gated() -> Rule {
    Rule {
        path_prefix: "/api/oauth/profile".into(),
        respond: Respond::Gated(Box::new(profile_rule().respond)),
    }
}

fn head_with_windows() -> Respond {
    Respond::Json {
        status: 200,
        headers: vec![
            (
                "anthropic-ratelimit-unified-5h-utilization".into(),
                "0.77".into(),
            ),
            ("anthropic-ratelimit-unified-5h-reset".into(), "4600".into()),
            (
                "anthropic-ratelimit-unified-7d-utilization".into(),
                "0.2".into(),
            ),
            (
                "anthropic-ratelimit-unified-overage-in-use".into(),
                "false".into(),
            ),
        ],
        body: br#"{"id":"msg_1","type":"message","usage":{"input_tokens":1,"output_tokens":1}}"#
            .to_vec(),
    }
}

/// Where a scheduler test paused tokio's clock, and what the fake had seen by then: the steps count only the
/// connections accepted since, and a connection that stays open through them (a pooled inference connection,
/// a request held at the fake's gate) is one of the `idle`.
struct Paused {
    start: Instant,
    idle: Cell<usize>,
    accepted: usize,
    requests: usize,
    /// Connections accepted as of the last step that settled.
    seen: Cell<usize>,
}

/// Pauses tokio's clock, spawns the poll's scheduler over the harness's usage service as the router's start
/// does, and lets it take its first pass. Every polled account whose row holds no limit resets read reads as read at
/// `NOW`, so a scheduler test meets no block read inside its first hour unless it stores an instant of its own first:
/// those tests pin the poll's cadence by every request and connection the fake sees, and a block read in an account's
/// first slot would add one of each.
async fn start_polling(h: &Harness, up: &FakeUpstream) -> Paused {
    for row in h.store.accounts().unwrap() {
        if phoebis_ledger::polled(row.kind, row.state) {
            stamp_limit_resets_read(&h.store, &row.id);
        }
    }
    tokio::time::pause();
    let paused = Paused {
        start: Instant::now(),
        idle: Cell::new(up.open_connections()),
        accepted: up.connections(),
        requests: up.requests().len(),
        seen: Cell::new(up.connections()),
    };
    h.ledger.track(
        QuotaPoll {
            ledger: h.ledger.clone(),
            router: h.router.clone(),
            store: h.store.clone(),
            tables: Arc::new(PlanTables::shipped()),
            usage: h
                .usage
                .clone()
                .expect("the harness was built with the poll"),
        }
        .spawn(),
    );
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    paused
}

/// Moves the paused tokio clock and the harness's clock cell together to `to` past the start, one step per
/// second so no step passes more than one slot and each attempt is drawn at its own. Each step lands a
/// millisecond past its second, as tokio rounds a deadline up to its millisecond: a slot due on the second
/// fires in its own step. After each step the attempts it drew run their socket I/O with time moving,
/// bounded, until every connection the fake accepted has sent its one request and closed (a control request
/// opens its own and drops it once the answer is read), and time stops again. A single large `advance` would
/// draw every slot it passed at its end, and a paused clock left idle over socket I/O auto-advances into the
/// control client's timeout.
async fn step_to(h: &Harness, up: &FakeUpstream, paused: &Paused, to: Duration) {
    let target = paused.start + to;
    let sync = || {
        let elapsed = Instant::now().duration_since(paused.start).as_millis() as i64;
        h.clock.store(NOW + elapsed, Ordering::SeqCst);
    };
    loop {
        let now = Instant::now();
        if now >= target {
            break;
        }
        let second = now
            .duration_since(paused.start)
            .saturating_sub(Duration::from_millis(1))
            .as_secs()
            + 1;
        let next =
            (paused.start + Duration::from_secs(second) + Duration::from_millis(1)).min(target);
        tokio::time::advance(next - now).await;
        sync();
        loop {
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            if up.connections() == paused.seen.get() && up.open_connections() <= paused.idle.get() {
                break;
            }
            tokio::time::resume();
            let accepted = settled(up, paused).await;
            tokio::time::pause();
            paused.seen.set(accepted);
            sync();
        }
    }
}

/// Waits with time moving until every connection the fake accepted since the start has sent its one request and only
/// the idle ones stay open, and answers the connections accepted in all. The shared `wait_for` panics with no count,
/// so this deadline is its own: a step that does not settle under host load names the counts that did not.
async fn settled(up: &FakeUpstream, paused: &Paused) -> usize {
    let deadline = Duration::from_secs(30);
    let give_up = std::time::Instant::now() + deadline;
    loop {
        let accepted = up.connections();
        let answered = up.requests().len() - paused.requests;
        let open = up.open_connections();
        let idle = paused.idle.get();
        let since = accepted - paused.accepted;
        if open <= idle && since == answered {
            return accepted;
        }
        assert!(
            std::time::Instant::now() < give_up,
            "the fake upstream did not settle within {deadline:?}\n\
             accepted connections since the start: {since}, expected {answered}, one per answered request\n\
             answered requests since the start: {answered}, expected {since}, one per accepted connection\n\
             open connections: {open}, expected at most {idle}, the idle ones\n\
             idle connections: {idle}, expected at least {open}, the open ones"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Runs `wait` with time moving, for a check on something a background task lands (a ledger row, a router
/// reply), then stops time again.
async fn resumed<T>(wait: impl Future<Output = T>) -> T {
    tokio::time::resume();
    let answer = wait.await;
    tokio::time::pause();
    answer
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// Requests to `path` carrying `secret`.
fn asked(up: &FakeUpstream, path: &str, secret: &str) -> usize {
    up.requests()
        .iter()
        .filter(|r| r.line.starts_with(&format!("GET {path}")) && r.credential() == Some(secret))
        .count()
}

/// Usage reads on the plain path: the space after the target keeps the client's shape out.
fn usage_asked(up: &FakeUpstream, secret: &str) -> usize {
    asked(up, "/api/oauth/usage ", secret)
}

fn resets_asked(up: &FakeUpstream, secret: &str) -> usize {
    asked(up, LIMIT_RESETS_TARGET, secret)
}

/// Stamps `NOW` as the account's last limit resets read, with no block, where its row holds no read.
fn stamp_limit_resets_read(store: &Store, id: &str) {
    let row = store.account(id).unwrap().unwrap();
    let extras = store.account_extras_of(id).unwrap().unwrap();
    if extras.limit_resets_read_at.is_none() {
        store
            .set_limit_resets(
                id,
                row.incarnation,
                row.lifecycle_generation,
                extras.limit_resets_epoch,
                None,
                NOW,
            )
            .unwrap();
    }
}

/// The typed block the parser answers for `BLOCK`, as the usage service stores it.
fn stored_block() -> String {
    serde_json::to_string(&phoebis_oauth::parse_limit_resets(BLOCK).unwrap().unwrap()).unwrap()
}

/// Writes `block` and its read's instant to the row under the row's holder, at the epoch of a row no pairing moved.
fn store_block(h: &Harness, id: &str, block: &str, read_at: i64) {
    let row = h.store.account(id).unwrap().unwrap();
    assert!(h
        .store
        .set_limit_resets(
            id,
            row.incarnation,
            row.lifecycle_generation,
            0,
            Some(block),
            read_at
        )
        .unwrap());
}

fn rate_limited_bare() -> Respond {
    Respond::Json {
        status: 429,
        headers: vec![],
        body: b"{}".to_vec(),
    }
}

fn rate_limited_for(retry_after_seconds: &str) -> Respond {
    Respond::Json {
        status: 429,
        headers: vec![("retry-after".into(), retry_after_seconds.into())],
        body: b"{}".to_vec(),
    }
}

/// Pairs `id` into the store as the pairing flow leaves it: a ready OAuth row, its incarnation the store's
/// next for the id, and a bundle far from expiry. Its row holds a limit resets read at `NOW`, as `start_polling`
/// stamps one, since a block read in the account's first slots would add a request and a connection to the counts
/// that pin the poll's cadence.
fn pair(store: &Store, id: &str, secret: &str) {
    store
        .insert_account(&AccountRow {
            identity: Some(Identity {
                account_uuid: "00000000-0000-4000-8000-000000000199".into(),
                email: format!("{id}@example.invalid"),
                organization_uuid: None,
            }),
            ..AccountRow::for_tests(id, AccountKind::Oauth)
        })
        .unwrap();
    let incarnation = store.account(id).unwrap().unwrap().incarnation;
    store
        .commit_credentials(
            id,
            incarnation,
            1,
            None,
            None,
            &NewBundle {
                access_token: Secret::new(secret),
                refresh_token: Some(Secret::new(&format!("{secret}-refresh"))),
                issued_at: NOW,
                expires_at: i64::MAX / 2,
                refresh_expires_at: None,
                scopes: "user:inference".into(),
            },
        )
        .unwrap();
    stamp_limit_resets_read(store, id);
}

/// Seconds past `NOW` on the harness's clock cell, for a time the usage service stamped.
fn secs_past_now(at_ms: i64) -> f64 {
    (at_ms - NOW) as f64 / 1_000.0
}

#[tokio::test]
async fn a_head_and_a_poll_in_one_bucket_land_one_row_per_window_and_the_head_keeps_its_rows() {
    let up = FakeUpstream::start(vec![
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: head_with_windows(),
        },
        usage_rule(),
        profile_rule(),
    ])
    .await;
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    // The head lands in the bucket of the first scheduled read, one interval after the scheduler starts.
    h.clock.store(NOW + 250_000, Ordering::SeqCst);
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.into_body().collect().await.unwrap();
    let bucket = phoebis_ledger::bucket_of(NOW + 250_000);
    let head = wait_for_async(Duration::from_secs(5), || async {
        let rows = h.db.quota_samples("a", bucket, bucket + 300).await.unwrap();
        rows.iter()
            .any(|r| r.window == "five_hour" && r.source == SampleSource::Head)
            .then_some(rows)
    })
    .await;
    let five = head.iter().find(|r| r.window == "five_hour").unwrap();
    assert_eq!(
        (five.utilization, five.reset_at, five.bucket),
        (Some(0.77), Some(4_600_000), bucket)
    );
    assert!(head
        .iter()
        .any(|r| r.window == "seven_day" && r.utilization == Some(0.2)));
    let extra =
        h.db.extra_usage_samples("a", bucket, bucket + 300)
            .await
            .unwrap();
    assert_eq!(
        (
            extra[0].source.as_str(),
            extra[0].in_use,
            extra[0].used_credits
        ),
        ("head", Some(false), None),
        "a head carries no credits"
    );

    // The head landed first; the account's first slot falls in the same bucket. Both samplers spell a window by
    // the endpoint's key, so each window holds one row: the head's where the head read it, the poll's where
    // only the poll did, and the credits wait.
    let paused = start_polling(&h, &up).await;
    step_to(&h, &up, &paused, secs(302)).await;
    assert_eq!(usage_asked(&up, A), 1, "the first slot");
    let rows = resumed(wait_for_async(Duration::from_secs(5), || async {
        let rows = h.db.quota_samples("a", bucket, bucket + 300).await.unwrap();
        rows.iter()
            .any(|r| r.window == "seven_day_fable")
            .then_some(rows)
    }))
    .await;
    let mut windows: Vec<(&str, &str, Option<f64>)> = rows
        .iter()
        .map(|r| (r.window.as_str(), r.source.as_str(), r.utilization))
        .collect();
    windows.sort_by(|a, b| a.0.cmp(b.0));
    assert_eq!(
        windows,
        [
            ("five_hour", "head", Some(0.77)),
            ("seven_day", "head", Some(0.2)),
            ("seven_day_fable", "poll", Some(0.42)),
        ],
        "one row per window: the head's readings survive the poll, the Fable week comes from limits[] and \
         not from the named key"
    );
    let extra = resumed(h.db.extra_usage_samples("a", bucket, bucket + 300))
        .await
        .unwrap();
    assert_eq!(
        extra
            .iter()
            .map(|e| (e.source.as_str(), e.used_credits))
            .collect::<Vec<_>>(),
        [("head", None), ("poll", Some(1_234))],
        "the head row keeps its bucket, and the poll lands its credits on a row of its own beside it"
    );

    // The next slot, an interval on, lands in the next bucket and writes the credits there.
    step_to(&h, &up, &paused, secs(602)).await;
    assert_eq!(usage_asked(&up, A), 2, "the second slot");
    let next = bucket + 300;
    let extra = resumed(wait_for_async(Duration::from_secs(5), || async {
        let rows =
            h.db.extra_usage_samples("a", next, next + 300)
                .await
                .unwrap();
        (!rows.is_empty()).then_some(rows)
    }))
    .await;
    assert_eq!(
        (
            extra[0].source.as_str(),
            extra[0].monthly_limit,
            extra[0].used_credits,
            extra[0].utilization
        ),
        ("poll", Some(5_000), Some(1_234), Some(0.2468 / 100.0)),
        "extra-usage credits come from the poll only"
    );
    up.shutdown().await;
}

/// A head row and a poll row of one bucket both stand whichever lands first: the samples answer both, the head's share
/// and the poll's amounts each on its own row, and the account's latest reading is the bucket's head row, one row per
/// account.
#[tokio::test]
async fn a_head_and_a_poll_of_one_bucket_both_stand_in_either_order_and_the_latest_reading_is_the_head(
) {
    let now = NOW + 600_000;
    let bucket = phoebis_ledger::bucket_of(now);
    let head = phoebis_ledger::ExtraUsageSampleRecord {
        account_id: "a".into(),
        bucket,
        is_enabled: true,
        in_use: Some(false),
        utilization: Some(0.4),
        overage_reset: Some(now + 3_600_000),
        source: SampleSource::Head,
        ..Default::default()
    };
    let poll = phoebis_ledger::ExtraUsageSampleRecord {
        account_id: "a".into(),
        bucket,
        is_enabled: true,
        monthly_limit: Some(5_000),
        used_credits: Some(1_234),
        utilization: Some(0.25),
        currency: Some("USD".into()),
        source: SampleSource::Poll,
        ..Default::default()
    };
    for (first, second, order) in [
        (&head, &poll, "the head first"),
        (&poll, &head, "the poll first"),
    ] {
        let db = Arc::new(LedgerDb::open_in_memory().unwrap());
        db.insert_samples(&[], Some(first)).unwrap();
        db.insert_samples(&[], Some(second)).unwrap();
        assert_eq!(
            db.extra_usage_samples("a", bucket, bucket + 300)
                .await
                .unwrap(),
            [head.clone(), poll.clone()],
            "{order}: neither hides the other"
        );
        let latest = db
            .read_blocking(move |c| phoebis_ledger::latest_extra_usage(c, now))
            .unwrap();
        assert_eq!(
            latest,
            std::slice::from_ref(&head),
            "{order}: the bucket's head row is the account's one latest reading"
        );
    }
}

#[tokio::test]
async fn every_polled_account_is_read_once_per_interval_on_its_own_phase_and_reported() {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    let h = harness_with_poll(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1), oauth("c", OAUTH_C, 2)],
        (0, None),
        LedgerConfig::default(),
    )
    .await;
    // A pause leaves the account polled; an unpaired one holds no credential worth asking with.
    h.store.set_enabled("a", false, "test").unwrap();
    h.store
        .set_state(
            "b",
            phoebis_store::AccountState::Unpaired,
            StateCause::Operator,
        )
        .unwrap();
    h.router.reload().await.unwrap();
    let profiles = |up: &FakeUpstream| {
        up.requests()
            .iter()
            .filter(|r| r.line.starts_with("GET /api/oauth/profile"))
            .count()
    };

    // With `a` and `c` polled, their phases are 0 and 150 s; the startup step has just read both, so each
    // first scheduled read comes an interval on, at 300 and 450 s.
    let paused = start_polling(&h, &up).await;
    step_to(&h, &up, &paused, secs(299)).await;
    assert_eq!(
        (usage_asked(&up, A), usage_asked(&up, OAUTH_C)),
        (0, 0),
        "nothing inside the first interval"
    );
    step_to(&h, &up, &paused, secs(302)).await;
    assert_eq!(
        (usage_asked(&up, A), usage_asked(&up, OAUTH_C)),
        (1, 0),
        "a alone by 302 s"
    );
    step_to(&h, &up, &paused, secs(452)).await;
    assert_eq!(
        (usage_asked(&up, A), usage_asked(&up, OAUTH_C)),
        (1, 1),
        "both once by 452 s"
    );
    assert_eq!(
        profiles(&up),
        0,
        "the startup step's profile read counts as the first"
    );
    step_to(&h, &up, &paused, secs(602)).await;
    assert_eq!(
        (
            usage_asked(&up, A),
            usage_asked(&up, OAUTH_C),
            usage_asked(&up, B)
        ),
        (2, 1, 0),
        "a twice and c once by 602 s; the unpaired b never"
    );

    let samples = resumed(h.router.samples()).await.unwrap();
    let (_, sample) = samples.iter().find(|(id, _)| id == "a").unwrap();
    assert_eq!(
        sample.reading(FIVE_HOUR).map(|r| r.utilization),
        Some(0.125),
        "the poll's reading reached the router"
    );
    let overage = resumed(h.router.overage("a")).await.unwrap().unwrap();
    assert_eq!(
        (overage.in_use, overage.source),
        (false, EvidenceSource::Sample),
        "the poll's overage evidence reached the router"
    );

    // With the startup step's read as the first, the third scheduled poll of each account reads the profile
    // again, a quarter hour after the startup step.
    let interval = POLL_INTERVAL.as_secs();
    step_to(&h, &up, &paused, secs(452 + interval)).await;
    assert_eq!((usage_asked(&up, A), usage_asked(&up, OAUTH_C)), (2, 2));
    assert_eq!(profiles(&up), 0, "none before the third poll");
    step_to(&h, &up, &paused, secs(452 + 2 * interval)).await;
    assert_eq!((usage_asked(&up, A), usage_asked(&up, OAUTH_C)), (3, 3));
    assert_eq!(profiles(&up), 2, "the third poll of each reads the profile");
    assert_eq!(usage_asked(&up, B), 0);
    assert!(
        h.store
            .account_extras_of("b")
            .unwrap()
            .and_then(|extras| extras.profile)
            .is_none(),
        "an unpaired account is not polled"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_rate_limited_account_backs_off_alone_while_its_sibling_keeps_the_interval() {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    up.add_credential_rule(A, "/api/oauth/usage", rate_limited_bare());
    let h = harness_with_poll(
        &up,
        &[oauth("a", A, 0), oauth("c", OAUTH_C, 1)],
        (0, None),
        LedgerConfig::default(),
    )
    .await;
    let (lines, _sink) = capture_tracing();
    let paused = start_polling(&h, &up).await;

    // The hold grows through 2, 4 and 8 minutes to 15, and the next attempt comes when the hold ends: `a` at
    // 300 s, then 120, 240, 480, 900 and 900 s on (about 421, 662, 1 143, 2 044 and 2 945 s, a second or so late
    // each); `c` every 300 s from 450 s throughout.
    step_to(&h, &up, &paused, secs(599)).await;
    assert_eq!(
        usage_asked(&up, A),
        2,
        "the two-minute hold ends before the next slot"
    );
    step_to(&h, &up, &paused, secs(1_205)).await;
    assert_eq!(
        usage_asked(&up, A),
        4,
        "the holds shorter than the interval end on their own"
    );
    step_to(&h, &up, &paused, secs(1_675)).await;
    assert_eq!(
        usage_asked(&up, A),
        4,
        "the fifteen-minute hold outlasts the next slot"
    );
    step_to(&h, &up, &paused, secs(3_000)).await;
    assert_eq!((usage_asked(&up, A), usage_asked(&up, OAUTH_C)), (6, 9));
    let status = h.usage.as_ref().unwrap().poll_status("a").unwrap();
    assert_eq!(
        (status.last_outcome, status.rate_limited_in_a_row),
        (PollOutcome::RateLimited, 6)
    );
    let warnings = lines
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.contains("rate-limited") && l.contains("account=a"))
        .count();
    assert_eq!(warnings, 6, "one warning per 429, naming the account");
    assert!(
        !lines.lock().unwrap().iter().any(|l| l.contains("sk-ant-")),
        "no token reaches a line"
    );

    // The next answer is clean: the run ends, and the request after it is one interval later.
    up.set_credential_rule_after(A, 0, "/api/oauth/usage", usage_rule().respond);
    step_to(&h, &up, &paused, secs(3_850)).await;
    assert_eq!(usage_asked(&up, A), 7);
    let status = h.usage.as_ref().unwrap().poll_status("a").unwrap();
    assert_eq!(
        (
            status.last_outcome,
            status.rate_limited_in_a_row,
            status.backoff_since,
            status.next_attempt_at
        ),
        (PollOutcome::Clean, 0, None, None)
    );
    let clean_at = secs_past_now(status.last_attempt_at).ceil() as u64;
    step_to(&h, &up, &paused, secs(clean_at + 297)).await;
    assert_eq!(usage_asked(&up, A), 7, "not before the interval");
    step_to(&h, &up, &paused, secs(clean_at + 303)).await;
    assert_eq!(
        usage_asked(&up, A),
        8,
        "one interval after the clean answer"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_retry_after_is_honoured_and_capped_by_the_poll() {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    up.add_credential_rule(A, "/api/oauth/usage", usage_rule().respond);
    up.push_credential_rule(A, "/api/oauth/usage", rate_limited_for("120"));
    up.push_credential_rule(A, "/api/oauth/usage", rate_limited_for("3600"));
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let paused = start_polling(&h, &up).await;

    step_to(&h, &up, &paused, secs(302)).await;
    assert_eq!(usage_asked(&up, A), 1, "the first slot");
    step_to(&h, &up, &paused, secs(418)).await;
    assert_eq!(
        usage_asked(&up, A),
        1,
        "the two minutes asked for are not stretched to the next slot"
    );
    step_to(&h, &up, &paused, secs(424)).await;
    assert_eq!(usage_asked(&up, A), 2, "asked again when the 120 s end");
    let status = h.usage.as_ref().unwrap().poll_status("a").unwrap();
    assert_eq!(
        status.next_attempt_at,
        Some(status.last_attempt_at + 900_000),
        "the hour asked for is held to fifteen minutes"
    );
    step_to(&h, &up, &paused, secs(1_318)).await;
    assert_eq!(usage_asked(&up, A), 2, "held inside the fifteen minutes");
    step_to(&h, &up, &paused, secs(1_326)).await;
    assert_eq!(usage_asked(&up, A), 3, "asked again at the cap");
    up.shutdown().await;
}

/// A scheduler that wakes several slots late, after a stall or a suspended process, reads once and takes the
/// cadence up at the next slot on the account's own phase: replaying each missed slot would ask an endpoint that
/// rate-limits per account several times back to back, and a cadence counted from the late read would move the
/// account off its phase for good.
#[tokio::test]
async fn a_wake_several_slots_late_reads_once_and_the_next_read_keeps_the_phase() {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let paused = start_polling(&h, &up).await;
    // The account's slots fall on 300 s and every interval after; the scheduler next runs at 1,049 s, past three
    // of them in one step.
    let late = Duration::from_millis(1_049_001);
    h.clock
        .store(NOW + late.as_millis() as i64, Ordering::SeqCst);
    tokio::time::advance(late).await;
    step_to(&h, &up, &paused, secs(1_052)).await;
    assert_eq!(usage_asked(&up, A), 1, "the late wake reads once");
    step_to(&h, &up, &paused, secs(1_352)).await;
    assert_eq!(
        usage_asked(&up, A),
        1,
        "not an interval after the late read, which is off the phase"
    );
    step_to(&h, &up, &paused, secs(1_498)).await;
    assert_eq!(usage_asked(&up, A), 1);
    step_to(&h, &up, &paused, secs(1_502)).await;
    assert_eq!(
        usage_asked(&up, A),
        2,
        "the next read at the account's next slot on its phase"
    );
    step_to(&h, &up, &paused, secs(1_798)).await;
    assert_eq!(usage_asked(&up, A), 2);
    step_to(&h, &up, &paused, secs(1_802)).await;
    assert_eq!(usage_asked(&up, A), 3, "and every interval after it");
    up.shutdown().await;
}

#[tokio::test]
async fn an_empty_schedule_wakes_every_interval_and_polls_a_paired_account_without_a_restart() {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    let h = harness_with_poll(&up, &[], (0, None), LedgerConfig::default()).await;
    let usage_requests = || {
        up.requests()
            .iter()
            .filter(|r| r.line.starts_with("GET /api/oauth/usage "))
            .count()
    };
    let paused = start_polling(&h, &up).await;
    step_to(&h, &up, &paused, secs(450)).await;
    assert_eq!(usage_requests(), 0, "nothing to poll");

    // Paired half an interval after the last pass: the next pass finds it.
    pair(&h.store, "a", A);
    step_to(&h, &up, &paused, secs(450 + 300)).await;
    assert_eq!(
        usage_asked(&up, A),
        1,
        "found and asked within one interval"
    );

    h.store.delete_account("a", "test").unwrap();
    step_to(&h, &up, &paused, secs(750 + 600)).await;
    assert_eq!(usage_requests(), 1, "a deleted account is not asked again");

    pair(&h.store, "d", OAUTH_D);
    step_to(&h, &up, &paused, secs(1_350 + 300)).await;
    assert_eq!(
        usage_asked(&up, OAUTH_D),
        1,
        "found and asked within one interval of the schedule going empty"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_pairing_re_spreads_the_slots_without_reading_an_account_again_within_its_interval() {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    let h = harness_with_poll(
        &up,
        &[oauth("a", A, 0), oauth("c", OAUTH_C, 1)],
        (0, None),
        LedgerConfig::default(),
    )
    .await;
    let paused = start_polling(&h, &up).await;
    step_to(&h, &up, &paused, secs(302)).await;
    assert_eq!((usage_asked(&up, A), usage_asked(&up, OAUTH_C)), (1, 0));

    // Paired ten seconds after `a`'s read; `c`'s slot at 450 s makes the pass that re-spreads the three.
    step_to(&h, &up, &paused, secs(310)).await;
    pair(&h.store, "b", B);
    step_to(&h, &up, &paused, secs(452)).await;
    assert_eq!(
        (usage_asked(&up, A), usage_asked(&up, B)),
        (1, 0),
        "the re-spread reads a no sooner than its interval, and b at its phase"
    );
    step_to(&h, &up, &paused, secs(598)).await;
    assert_eq!(
        (usage_asked(&up, A), usage_asked(&up, B)),
        (1, 1),
        "b read at its phase, 100 s after the pass"
    );
    step_to(&h, &up, &paused, secs(602)).await;
    assert_eq!(
        usage_asked(&up, A),
        2,
        "a read again an interval after its last read"
    );
    assert_eq!(
        usage_asked(&up, OAUTH_C),
        0,
        "c's due slot took its new phase, 200 s after the pass"
    );
    step_to(&h, &up, &paused, secs(652)).await;
    assert_eq!(usage_asked(&up, OAUTH_C), 1);
    up.shutdown().await;
}

#[tokio::test]
async fn a_re_spread_during_a_hold_reads_the_held_account_when_its_hold_ends() {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    up.add_credential_rule(A, "/api/oauth/usage", rate_limited_bare());
    let h = harness_with_poll(
        &up,
        &[
            oauth("a", A, 0),
            oauth("c", OAUTH_C, 1),
            oauth("d", OAUTH_D, 2),
        ],
        (0, None),
        LedgerConfig::default(),
    )
    .await;
    let paused = start_polling(&h, &up).await;
    // `a` is answered 429 at its 300 s slot and held two minutes, to about 421 s.
    step_to(&h, &up, &paused, secs(302)).await;
    assert_eq!(usage_asked(&up, A), 1);

    // Paired during the hold; `c`'s slot at 400 s makes the pass that re-spreads the four, with `a` still held.
    step_to(&h, &up, &paused, secs(310)).await;
    pair(&h.store, "b", B);
    step_to(&h, &up, &paused, secs(418)).await;
    assert_eq!(
        (usage_asked(&up, A), usage_asked(&up, B)),
        (1, 0),
        "not before the hold ends"
    );
    step_to(&h, &up, &paused, secs(425)).await;
    assert_eq!(
        usage_asked(&up, A),
        2,
        "the re-spread keeps the hold's end, not the interval after the 429"
    );
    step_to(&h, &up, &paused, secs(480)).await;
    assert_eq!(
        usage_asked(&up, B),
        1,
        "b read at its phase, 75 s after the pass"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_registry_read_that_fails_keeps_the_schedule_and_logs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let store = Arc::new(Store::open(&path, &StorageKey::generate()).unwrap());
    let db = Arc::new(LedgerDb::open_in_memory().unwrap());
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    let h = harness_on(
        store,
        db,
        &up,
        &[oauth("a", A, 0), oauth("c", OAUTH_C, 1)],
        (0, None),
        LedgerConfig::default(),
        true,
    )
    .await;
    let (lines, _sink) = capture_tracing();
    let registry_warnings = || {
        lines
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.contains("the registry could not be read"))
            .cloned()
            .collect::<Vec<_>>()
    };
    let paused = start_polling(&h, &up).await;
    step_to(&h, &up, &paused, secs(452)).await;
    assert_eq!((usage_asked(&up, A), usage_asked(&up, OAUTH_C)), (1, 1));

    // A second connection on the store's file takes the table away.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.busy_timeout(Duration::from_secs(5)).unwrap();
    conn.execute_batch("ALTER TABLE accounts RENAME TO accounts_gone")
        .unwrap();
    step_to(&h, &up, &paused, secs(752)).await;
    // Neither account's row can be read to fetch with, so the attempts fail before a request; the service
    // still saw each on its phase.
    let usage = h.usage.as_ref().unwrap();
    let attempted = |id: &str| secs_past_now(usage.poll_status(id).unwrap().last_attempt_at);
    assert!(
        (599.5..603.5).contains(&attempted("a")) && (749.5..752.5).contains(&attempted("c")),
        "both asked on their phases: a at {}, c at {}",
        attempted("a"),
        attempted("c")
    );
    // Four passes failed to read (two slots, two completions); the outage is logged once.
    let warned = registry_warnings();
    assert_eq!(warned.len(), 1, "one warning for the outage: {warned:?}");
    assert!(
        warned.iter().all(|l| !l.contains("sk-ant-")),
        "no token in the warning: {warned:?}"
    );

    conn.execute_batch("ALTER TABLE accounts_gone RENAME TO accounts")
        .unwrap();
    step_to(&h, &up, &paused, secs(1_052)).await;
    assert_eq!(
        (usage_asked(&up, A), usage_asked(&up, OAUTH_C)),
        (2, 2),
        "the next passes read the table again and the requests go out on the same phases"
    );
    assert_eq!(
        registry_warnings().len(),
        1,
        "no warning once the table is back"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_completion_drawn_for_a_deleted_holder_moves_no_slot_of_its_replacement() {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    // `c`'s first answer, at 450 s, holds it until two seconds before `a`'s slot at 900 s; that second attempt is
    // held at the fake's gate across the pass `a`'s slot makes, which finds `c` paired again.
    up.push_credential_rule(OAUTH_C, "/api/oauth/usage", rate_limited_for("447"));
    up.push_credential_rule(
        OAUTH_C,
        "/api/oauth/usage",
        Respond::Gated(Box::new(usage_rule().respond)),
    );
    let h = harness_with_poll(
        &up,
        &[oauth("a", A, 0), oauth("c", OAUTH_C, 1)],
        (0, None),
        LedgerConfig::default(),
    )
    .await;
    let paused = start_polling(&h, &up).await;
    step_to(&h, &up, &paused, secs(897)).await;
    assert_eq!((usage_asked(&up, A), usage_asked(&up, OAUTH_C)), (2, 1));
    paused.idle.set(paused.idle.get() + 1);
    step_to(&h, &up, &paused, secs(899)).await;
    assert_eq!(
        usage_asked(&up, OAUTH_C),
        2,
        "the second attempt is out, held at the gate"
    );

    h.store.delete_account("c", "test").unwrap();
    pair(&h.store, "c", REPAIRED);
    assert_eq!(h.store.account("c").unwrap().unwrap().incarnation, 2);
    // `a`'s slot makes the pass that re-spreads: the replacement is drawn half an interval on.
    step_to(&h, &up, &paused, Duration::from_millis(900_500)).await;
    assert_eq!(usage_asked(&up, A), 3, "a asked at its slot");

    // The old attempt completes after that pass; its completion moves nothing, so the replacement keeps the
    // slot the pass drew it.
    up.release();
    paused.idle.set(paused.idle.get() - 1);
    resumed(wait_for(Duration::from_secs(30), || {
        (up.open_connections() <= paused.idle.get()).then_some(())
    }))
    .await;
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    step_to(&h, &up, &paused, secs(1_048)).await;
    assert_eq!(usage_asked(&up, REPAIRED), 0, "not before its phase");
    step_to(&h, &up, &paused, secs(1_053)).await;
    assert_eq!(
        usage_asked(&up, REPAIRED),
        1,
        "the replacement asked at its own slot, with its own token"
    );
    step_to(&h, &up, &paused, secs(1_349)).await;
    assert_eq!(usage_asked(&up, REPAIRED), 1);
    step_to(&h, &up, &paused, secs(1_355)).await;
    assert_eq!(
        (usage_asked(&up, OAUTH_C), usage_asked(&up, REPAIRED)),
        (2, 2),
        "the replacement's slot moved by its own completion; the deleted holder is never asked again"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn the_scheduler_reaps_its_attempts_and_the_ledger_tracks_none_of_them() {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    let h = harness_with_poll(
        &up,
        &[oauth("a", A, 0), oauth("c", OAUTH_C, 1)],
        (0, None),
        LedgerConfig::default(),
    )
    .await;
    let mirror = h.ledger.tracked();
    let sampled = QuotaPoll {
        ledger: h.ledger.clone(),
        router: h.router.clone(),
        store: h.store.clone(),
        tables: Arc::new(PlanTables::shipped()),
        usage: h.usage.clone().unwrap(),
    }
    .sample_all(Duration::from_secs(2))
    .await;
    assert_eq!(sampled.reported, 2);
    assert_eq!(
        h.ledger.tracked(),
        mirror + 2,
        "the startup step's two fetches"
    );
    let paused = start_polling(&h, &up).await;
    let before = h.ledger.tracked();
    assert_eq!(before, mirror + 3, "and the scheduler");
    let profiles = || {
        up.requests()
            .iter()
            .filter(|r| r.line.starts_with("GET /api/oauth/profile"))
            .count()
    };
    step_to(&h, &up, &paused, secs(10 * POLL_INTERVAL.as_secs() + 152)).await;
    assert_eq!(
        (usage_asked(&up, A), usage_asked(&up, OAUTH_C)),
        (11, 11),
        "the startup read and ten slots each"
    );
    assert_eq!(
        profiles(),
        8,
        "the startup step's two reads and the third, sixth and ninth slots of each"
    );
    assert_eq!(
        h.ledger.tracked(),
        before,
        "no attempt is tracked by the ledger"
    );
    up.shutdown().await;
}

/// A startup read the endpoint refuses counts under `failed`, apart from the account whose sample landed.
#[tokio::test]
async fn the_startup_step_counts_an_account_the_endpoint_refused_as_failed() {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    up.add_credential_rule(OAUTH_C, "/api/oauth/usage", rate_limited_bare());
    let h = harness_with_poll(
        &up,
        &[oauth("a", A, 0), oauth("c", OAUTH_C, 1)],
        (0, None),
        LedgerConfig::default(),
    )
    .await;
    let sampled = QuotaPoll {
        ledger: h.ledger.clone(),
        router: h.router.clone(),
        store: h.store.clone(),
        tables: Arc::new(PlanTables::shipped()),
        usage: h.usage.clone().unwrap(),
    }
    .sample_all(Duration::from_secs(2))
    .await;
    assert_eq!(
        (sampled.reported, sampled.failed, sampled.pending),
        (1, 1, 0),
        "the 429 is a failed read beside the reported one"
    );
    up.shutdown().await;
}

/// The token endpoint's answer to a refresh token it no longer honors.
fn invalid_grant() -> Rule {
    Rule {
        path_prefix: "/v1/oauth/token".into(),
        respond: Respond::Json {
            status: 400,
            headers: vec![],
            body: br#"{"error":"invalid_grant","error_description":"revoked"}"#.to_vec(),
        },
    }
}

/// Commits a bundle for `id` that sits inside its refresh margin at `NOW`, so the next acquisition goes to the token
/// endpoint rather than answering the cached token.
fn due_for_refresh(store: &Store, id: &str) {
    let stored = store.credentials(id).unwrap().unwrap();
    store
        .commit_credentials(
            id,
            1,
            1,
            Some(stored.generation),
            None,
            &NewBundle {
                access_token: Secret::new("sk-ant-oat01-SHORT"),
                refresh_token: Some(Secret::new("sk-ant-oat01-SHORT-refresh")),
                issued_at: NOW - 3_600_000,
                expires_at: NOW + 60_000,
                refresh_expires_at: None,
                scopes: "user:inference".into(),
            },
        )
        .unwrap();
}

/// Waits for the router's snapshot to read `id` as `reauth_required`. The manager's mark is a store write alone, which
/// starts no registry reload, so only a report reaches the snapshot.
async fn routing_reads_reauth_required(h: &Harness, id: &str) {
    wait_for(Duration::from_secs(30), || {
        h.router
            .snapshot()
            .account(id)
            .filter(|row| row.state == AccountState::ReauthRequired)
            .map(|_| ())
    })
    .await;
}

#[tokio::test]
async fn a_poll_whose_refresh_lands_a_reauth_mark_reports_it_to_routing_at_once() {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule(), invalid_grant()]).await;
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    due_for_refresh(&h.store, "a");
    let row = h.store.account("a").unwrap().unwrap();
    let answer = QuotaPoll {
        ledger: h.ledger.clone(),
        router: h.router.clone(),
        store: h.store.clone(),
        tables: Arc::new(PlanTables::shipped()),
        usage: h.usage.clone().unwrap(),
    }
    .poll_account(&row)
    .await
    .unwrap();
    assert!(answer.error.is_some(), "the poll read nothing");
    assert_eq!(
        h.store.account("a").unwrap().unwrap().state,
        AccountState::ReauthRequired,
        "the refresh's invalid_grant landed the mark"
    );
    routing_reads_reauth_required(&h, "a").await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_profile_refresh_whose_refresh_lands_a_reauth_mark_reports_it_to_routing_at_once() {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule(), invalid_grant()]).await;
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    due_for_refresh(&h.store, "a");
    let row = h.store.account("a").unwrap().unwrap();
    QuotaPoll {
        ledger: h.ledger.clone(),
        router: h.router.clone(),
        store: h.store.clone(),
        tables: Arc::new(PlanTables::shipped()),
        usage: h.usage.clone().unwrap(),
    }
    .refresh_profile(&row)
    .await;
    assert_eq!(
        h.store.account("a").unwrap().unwrap().state,
        AccountState::ReauthRequired,
        "the refresh's invalid_grant landed the mark"
    );
    routing_reads_reauth_required(&h, "a").await;
    up.shutdown().await;
}

/// A profile answer whose seat tier names a usage-based Enterprise seat under the plan tier the case stores first.
fn usage_based_seat_profile_rule() -> Rule {
    Rule {
        path_prefix: "/api/oauth/profile".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"account":{"uuid":"00000000-0000-4000-8000-000000000100","email":"a@example.invalid"},"organization":{"uuid":"o","rate_limit_tier":"default_claude_max_5x","seat_tier":"enterprise_usage_based"}}"#
                .to_vec(),
        },
    }
}

#[tokio::test]
async fn a_profile_refresh_whose_seat_tier_alone_moves_the_class_reloads_the_router() {
    let up = FakeUpstream::start(vec![usage_rule(), usage_based_seat_profile_rule()]).await;
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    // The refresh repeats the stored tier, so only the seat tier moves.
    h.store
        .set_profile(
            "a",
            r#"{"organization":{"rate_limit_tier":"default_claude_max_5x"}}"#,
            &SubscriptionRow {
                tier: Some("default_claude_max_5x".into()),
                ..SubscriptionRow::default()
            },
        )
        .unwrap();
    h.router.reload().await.unwrap();
    assert!(
        !h.router.snapshot().usage_based("a"),
        "a plan seat before the refresh"
    );
    let row = h.store.account("a").unwrap().unwrap();
    QuotaPoll {
        ledger: h.ledger.clone(),
        router: h.router.clone(),
        store: h.store.clone(),
        tables: Arc::new(PlanTables::shipped()),
        usage: h.usage.clone().unwrap(),
    }
    .refresh_profile(&row)
    .await;
    assert!(
        h.store.subscription_term("a").unwrap().unwrap().usage_based,
        "the stored document classes the seat"
    );
    assert!(
        h.router.snapshot().usage_based("a"),
        "the routing snapshot reads the class the refresh stored, with no other reload"
    );
    up.shutdown().await;
}

/// A profile answer whose organization type names a Team plan under the Pro tier the case stores first.
fn team_on_the_pro_tier_profile_rule() -> Rule {
    Rule {
        path_prefix: "/api/oauth/profile".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"account":{"uuid":"00000000-0000-4000-8000-000000000100","email":"a@example.invalid"},"organization":{"uuid":"o","organization_type":"claude_team","rate_limit_tier":"default_claude_ai"}}"#
                .to_vec(),
        },
    }
}

#[tokio::test]
async fn a_profile_refresh_whose_organization_type_alone_moves_the_plan_reloads_the_router() {
    let up = FakeUpstream::start(vec![usage_rule(), team_on_the_pro_tier_profile_rule()]).await;
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    // The refresh repeats the stored tier, so only the organization type moves.
    h.store
        .set_profile(
            "a",
            r#"{"organization":{"organization_type":"claude_pro","rate_limit_tier":"default_claude_ai"}}"#,
            &SubscriptionRow {
                tier: Some("default_claude_ai".into()),
                ..SubscriptionRow::default()
            },
        )
        .unwrap();
    h.router.reload().await.unwrap();
    assert_eq!(
        h.router.snapshot().plan("a"),
        Some("pro"),
        "a Pro plan before the refresh"
    );
    let row = h.store.account("a").unwrap().unwrap();
    QuotaPoll {
        ledger: h.ledger.clone(),
        router: h.router.clone(),
        store: h.store.clone(),
        tables: Arc::new(PlanTables::shipped()),
        usage: h.usage.clone().unwrap(),
    }
    .refresh_profile(&row)
    .await;
    assert_eq!(
        h.store
            .subscription_term("a")
            .unwrap()
            .unwrap()
            .plan
            .as_deref(),
        Some("team"),
        "the stored document names the Team plan"
    );
    assert_eq!(
        h.router.snapshot().plan("a"),
        Some("team"),
        "the routing snapshot reads the plan the refresh stored, with no other reload"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn the_startup_step_records_and_reports_one_sample_per_polled_account() {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    let h = harness_with_poll(
        &up,
        &[
            oauth("a", A, 0),
            oauth("b", B, 1),
            Account {
                id: "c",
                kind: AccountKind::ApiKey,
                secret: "sk-ant-api03-cccccccccccccccccccccccccccccccc",
                priority: 2,
            },
        ],
        (0, None),
        LedgerConfig::default(),
    )
    .await;
    // A pause leaves the account polled: its quota is still the operator's to watch.
    h.store.set_enabled("a", false, "test").unwrap();
    let sampled = QuotaPoll {
        ledger: h.ledger.clone(),
        router: h.router.clone(),
        store: h.store.clone(),
        tables: Arc::new(PlanTables::shipped()),
        usage: h
            .usage
            .clone()
            .expect("the harness was built with the poll"),
    }
    .sample_all(Duration::from_secs(2))
    .await;
    assert_eq!(
        sampled,
        Sampled {
            reported: 2,
            failed: 0,
            pending: 0,
            paused: 0
        }
    );
    let samples = h.router.samples().await.unwrap();
    for id in ["a", "b"] {
        let (_, sample) = samples.iter().find(|(named, _)| named == id).unwrap();
        assert_eq!(
            sample.reading(FIVE_HOUR).map(|r| r.utilization),
            Some(0.125),
            "{id}"
        );
        let overage = h.router.overage(id).await.unwrap().unwrap();
        assert!(!overage.in_use, "{id}");
        assert_eq!(overage.source, EvidenceSource::Sample, "{id}");
        assert_eq!(overage.observed_at, NOW, "{id}");
        let bucket = phoebis_ledger::bucket_of(NOW);
        let rows = wait_for_async(Duration::from_secs(5), || async {
            let rows = h.db.quota_samples(id, bucket, bucket + 300).await.unwrap();
            rows.iter()
                .any(|r| r.window == "seven_day_fable")
                .then_some(rows)
        })
        .await;
        for window in ["five_hour", "seven_day", "seven_day_fable"] {
            assert_eq!(
                rows.iter().filter(|r| r.window == window).count(),
                1,
                "{id} {window}"
            );
        }
    }
    let usages = up
        .requests()
        .iter()
        .filter(|r| r.line.starts_with("GET /api/oauth/usage "))
        .count();
    assert_eq!(usages, 2, "one usage request per OAuth account");
    wait_for(Duration::from_secs(2), || {
        let profiles = up
            .requests()
            .iter()
            .filter(|r| r.line.starts_with("GET /api/oauth/profile"))
            .count();
        (profiles == 2).then_some(())
    })
    .await;
    assert!(
        !up.requests()
            .iter()
            .any(|r| r.credential() == Some("sk-ant-api03-cccccccccccccccccccccccccccccccc")),
        "the API-key account was not asked"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_usage_endpoint_that_hangs_ends_the_startup_step_at_its_bound() {
    let up = FakeUpstream::start(vec![usage_gated(), profile_rule()]).await;
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    // The gated usage endpoint answers only after the release below, so the step can return only by leaving that
    // fetch pending at its bound.
    let sampled = QuotaPoll {
        ledger: h.ledger.clone(),
        router: h.router.clone(),
        store: h.store.clone(),
        tables: Arc::new(PlanTables::shipped()),
        usage: h
            .usage
            .clone()
            .expect("the harness was built with the poll"),
    }
    .sample_all(Duration::from_millis(300))
    .await;
    assert_eq!(
        sampled,
        Sampled {
            reported: 0,
            failed: 0,
            pending: 1,
            paused: 0
        }
    );
    up.release();
    wait_for_async(Duration::from_secs(2), || async {
        h.router
            .samples()
            .await
            .ok()
            .filter(|s| s.iter().any(|(id, _)| id == "a"))
    })
    .await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_profile_read_that_names_a_new_tier_reloads_the_router() {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    assert_eq!(h.router.snapshot().tier("a"), None, "no profile read yet");
    QuotaPoll {
        ledger: h.ledger.clone(),
        router: h.router.clone(),
        store: h.store.clone(),
        tables: Arc::new(PlanTables::shipped()),
        usage: h
            .usage
            .clone()
            .expect("the harness was built with the poll"),
    }
    .sample_all(Duration::from_secs(2))
    .await;
    let tier = wait_for(Duration::from_secs(2), || {
        h.router.snapshot().tier("a").map(str::to_string)
    })
    .await;
    assert_eq!(
        tier, "default_claude_max_5x",
        "the snapshot prices the account's windows through the tier the profile named"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_profile_endpoint_that_hangs_holds_no_reported_sample_back() {
    let up = FakeUpstream::start(vec![usage_rule(), profile_gated()]).await;
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let sampled = QuotaPoll {
        ledger: h.ledger.clone(),
        router: h.router.clone(),
        store: h.store.clone(),
        tables: Arc::new(PlanTables::shipped()),
        usage: h
            .usage
            .clone()
            .expect("the harness was built with the poll"),
    }
    .sample_all(Duration::from_millis(300))
    .await;
    assert_eq!(
        sampled,
        Sampled {
            reported: 1,
            failed: 0,
            pending: 0,
            paused: 0
        }
    );
    let samples = h.router.samples().await.unwrap();
    assert!(samples.iter().any(|(id, _)| id == "a"));
    up.release();
    wait_for(Duration::from_secs(2), || {
        h.store
            .account_extras_of("a")
            .unwrap()
            .and_then(|extras| extras.profile)
    })
    .await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_stalled_startup_fetch_is_aborted_by_ledger_shutdown_and_makes_no_further_request() {
    let up = FakeUpstream::start(vec![usage_gated(), profile_rule()]).await;
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let tables = Arc::new(PlanTables::shipped());
    let sampled = QuotaPoll {
        ledger: h.ledger.clone(),
        router: h.router.clone(),
        store: h.store.clone(),
        tables: tables.clone(),
        usage: h
            .usage
            .clone()
            .expect("the harness was built with the poll"),
    }
    .sample_all(Duration::from_millis(300))
    .await;
    assert_eq!(
        sampled,
        Sampled {
            reported: 0,
            failed: 0,
            pending: 1,
            paused: 0
        }
    );
    assert!(
        Arc::strong_count(&tables) > 1,
        "the stalled fetch task holds its clone of the tables until shutdown aborts it"
    );
    h.ledger.clone().shutdown().await.unwrap();
    // Released only now: an aborted task never reaches the profile half, even once the fetch it never
    // awaited the end of is free to finish.
    up.release();
    // The fetch task holds the one other handle on the tables, so the count falls to one once the task has ended.
    wait_for(Duration::from_secs(5), || {
        (Arc::strong_count(&tables) == 1).then_some(())
    })
    .await;
    assert_eq!(
        up.requests()
            .iter()
            .filter(|r| r.line.starts_with("GET /api/oauth/usage "))
            .count(),
        1,
        "the aborted fetch made no retry"
    );
    assert_eq!(
        up.requests()
            .iter()
            .filter(|r| r.line.starts_with("GET /api/oauth/profile"))
            .count(),
        0,
        "the aborted fetch never reached the profile endpoint"
    );
    up.shutdown().await;
}

/// The poll backoff rows the ledger holds, in order: the kind, the account, when, and the detail.
async fn backoff_rows(h: &Harness) -> Vec<(String, Option<String>, i64, serde_json::Value)> {
    h.db.events(0, i64::MAX, 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind.starts_with("poll_backoff_"))
        .map(|e| {
            let detail = serde_json::from_str(e.detail.as_deref().unwrap()).unwrap();
            (e.kind, e.account_id, e.at, detail)
        })
        .collect()
}

#[tokio::test]
async fn a_rate_limited_poll_writes_the_backoff_s_start_and_the_clean_one_its_end() {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    up.add_credential_rule(A, "/api/oauth/usage", usage_rule().respond);
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    // A clean read ahead of the run, the startup step's, which the run's start names.
    let usage = h
        .usage
        .clone()
        .expect("the harness was built with the poll");
    usage.poll("a").await.unwrap();
    let clean_at = usage.poll_status("a").unwrap().last_clean_at;
    assert!(clean_at.is_some());
    up.push_credential_rule(A, "/api/oauth/usage", rate_limited_bare());
    let paused = start_polling(&h, &up).await;

    // The first slot at 300 s meets the 429 and holds the account for the ladder's two minutes. The read when
    // the hold ends is clean.
    step_to(&h, &up, &paused, secs(302)).await;
    assert_eq!(usage_asked(&up, A), 2);
    step_to(&h, &up, &paused, secs(424)).await;
    assert_eq!(usage_asked(&up, A), 3);
    let rows = resumed(wait_for_async(Duration::from_secs(5), || async {
        let rows = backoff_rows(&h).await;
        (rows.len() == 2).then_some(rows)
    }))
    .await;
    let (started, ended) = (&rows[0], &rows[1]);
    assert_eq!(
        (
            started.0.as_str(),
            started.1.as_deref(),
            ended.0.as_str(),
            ended.1.as_deref()
        ),
        (
            "poll_backoff_started",
            Some("a"),
            "poll_backoff_ended",
            Some("a")
        )
    );
    assert_eq!(
        (
            started.3["endpoint"].as_str(),
            started.3["attempts"].as_u64(),
            started.3["next_attempt_at"].as_i64(),
            started.3["last_clean_poll_at"].as_i64()
        ),
        (Some("usage"), Some(1), Some(started.2 + 120_000), clean_at),
        "the first 429 of a run, held for the ladder's first step, after the last clean read"
    );
    assert_eq!(
        started.3["reason"].as_str(),
        Some("429"),
        "the reason is the status that opened the run, never the hold's sentence: {}",
        started.3
    );
    assert_eq!(
        (
            ended.3["attempts"].as_u64(),
            ended.3["duration_ms"].as_i64(),
            ended.3["resumed_at"].as_i64()
        ),
        (Some(1), Some(ended.2 - started.2), Some(ended.2)),
        "the run lasted from its first 429 to the clean answer"
    );
    assert!(ended.2 - started.2 >= 120_000);
    up.shutdown().await;
}

#[tokio::test]
async fn a_profile_429_starts_the_backoff_and_the_next_clean_usage_poll_ends_it() {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    up.add_credential_rule(A, "/api/oauth/profile", profile_rule().respond);
    up.push_credential_rule(A, "/api/oauth/profile", rate_limited_bare());
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let paused = start_polling(&h, &up).await;
    let usage = h
        .usage
        .clone()
        .expect("the harness was built with the poll");

    assert!(resumed(usage.profile("a")).await.is_err());
    let started = resumed(wait_for_async(Duration::from_secs(5), || async {
        backoff_rows(&h).await.pop()
    }))
    .await;
    assert_eq!(
        (
            started.0.as_str(),
            started.3["endpoint"].as_str(),
            started.3["reason"].as_str(),
            started.3["attempts"].as_u64()
        ),
        (
            "poll_backoff_started",
            Some("profile"),
            Some("429"),
            Some(1)
        ),
        "the profile endpoint's 429 starts the run"
    );
    assert_eq!(usage_asked(&up, A), 0, "before any usage request");

    // The two-minute hold has ended by the first slot at 300 s, whose usage answer is clean.
    step_to(&h, &up, &paused, secs(302)).await;
    assert_eq!(usage_asked(&up, A), 1);
    let rows = resumed(wait_for_async(Duration::from_secs(5), || async {
        let rows = backoff_rows(&h).await;
        (rows.len() == 2).then_some(rows)
    }))
    .await;
    assert_eq!(rows[0], started);
    assert_eq!(
        (
            rows[1].0.as_str(),
            rows[1].3["attempts"].as_u64(),
            rows[1].3["duration_ms"].as_i64()
        ),
        ("poll_backoff_ended", Some(1), Some(rows[1].2 - started.2))
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_run_the_poll_never_saw_start_ends_without_a_row() {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    up.add_credential_rule(A, "/api/oauth/usage", usage_rule().respond);
    up.push_credential_rule(A, "/api/oauth/usage", rate_limited_bare());
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let usage = h
        .usage
        .clone()
        .expect("the harness was built with the poll");
    // The startup step's read, before the scheduler registers its hook: the 429 opens a run no row starts.
    usage.poll("a").await.unwrap();
    assert!(usage.poll_status("a").unwrap().backoff_since.is_some());
    let paused = start_polling(&h, &up).await;

    // The scheduler's first read comes when the two-minute hold ends. It is clean and ends the run.
    step_to(&h, &up, &paused, secs(302)).await;
    assert_eq!(usage_asked(&up, A), 2);
    assert_eq!(usage.poll_status("a").unwrap().backoff_since, None);
    // The clean read's samples land behind any row the run's end would have written.
    resumed(wait_for_async(Duration::from_secs(5), || async {
        (!h.db
            .quota_samples("a", 0, i64::MAX)
            .await
            .unwrap()
            .is_empty())
        .then_some(())
    }))
    .await;
    assert_eq!(resumed(backoff_rows(&h)).await, []);
    up.shutdown().await;
}

#[tokio::test]
async fn a_run_after_an_invalidated_one_writes_its_own_start_and_end() {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    up.add_credential_rule(A, "/api/oauth/usage", usage_rule().respond);
    up.push_credential_rule(A, "/api/oauth/usage", rate_limited_bare());
    up.push_credential_rule(A, "/api/oauth/usage", rate_limited_bare());
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let _paused = start_polling(&h, &up).await;
    let usage = h
        .usage
        .clone()
        .expect("the harness was built with the poll");

    // A run starts, and a re-pairing drops the account's entry mid-run, so that run never ends.
    resumed(usage.poll("a")).await.unwrap();
    usage.invalidate("a");
    // A second run starts on the next 429 and ends on the clean read after its two-minute hold.
    resumed(usage.poll("a")).await.unwrap();
    h.clock.store(NOW + 121_000, Ordering::SeqCst);
    resumed(usage.poll("a")).await.unwrap();
    assert_eq!(usage_asked(&up, A), 3);
    let rows = resumed(wait_for_async(Duration::from_secs(5), || async {
        let rows = backoff_rows(&h).await;
        rows.iter()
            .any(|row| row.0 == "poll_backoff_ended")
            .then_some(rows)
    }))
    .await;
    assert_eq!(
        rows.iter().map(|row| row.0.as_str()).collect::<Vec<_>>(),
        [
            "poll_backoff_started",
            "poll_backoff_started",
            "poll_backoff_ended"
        ],
        "the second run's start is written beside the first's"
    );
    assert_eq!(
        rows[2].3["duration_ms"].as_i64(),
        Some(rows[2].2 - rows[1].2),
        "the end measures the second run"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn no_limit_resets_read_goes_to_an_account_whose_plan_ended() {
    let up = FakeUpstream::start(vec![limit_resets_rule(), usage_rule(), profile_rule()]).await;
    let h = harness_with_poll(
        &up,
        &[oauth("a", A, 0), oauth("c", OAUTH_C, 1)],
        (0, None),
        LedgerConfig::default(),
    )
    .await;
    store_block(&h, "a", &stored_block(), NOW - 7_200_000);
    store_block(&h, "c", &stored_block(), NOW - 7_200_000);
    h.store.set_plan_ended("a", NOW - 60_000).unwrap();
    let paused = start_polling(&h, &up).await;

    // The first slots, `a`'s at 300 s and `c`'s at 450 s, both find the last read more than two hours back.
    step_to(&h, &up, &paused, secs(452)).await;
    assert_eq!(
        (resets_asked(&up, A), resets_asked(&up, OAUTH_C)),
        (0, 1),
        "the flagged account is sent no read, its sibling one"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn the_limit_resets_are_read_once_an_hour() {
    let up = FakeUpstream::start(vec![limit_resets_rule(), usage_rule(), profile_rule()]).await;
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    store_block(&h, "a", &stored_block(), NOW - 3_240_000);
    let paused = start_polling(&h, &up).await;

    // The slots come every 300 s from 300 s, and every third one from 900 s reads the profile.
    step_to(&h, &up, &paused, secs(302)).await;
    assert_eq!(resets_asked(&up, A), 0, "59 minutes after the last read");
    step_to(&h, &up, &paused, secs(602)).await;
    assert_eq!(resets_asked(&up, A), 1, "64 minutes after it");
    step_to(&h, &up, &paused, secs(1_202)).await;
    assert_eq!(
        resets_asked(&up, A),
        1,
        "the 900 s slot reads the profile, and the 1 200 s slot lies ten minutes after the read"
    );
    // The hour after the 600 s read passes at the 4 200 s slot, or, where that read went out a few milliseconds
    // past its slot, at the 4 800 s slot, since the 4 500 s slot reads the profile.
    step_to(&h, &up, &paused, secs(4_802)).await;
    assert_eq!(resets_asked(&up, A), 2, "the second hour's read");
    up.shutdown().await;
}

#[tokio::test]
async fn a_new_limit_sends_no_limit_resets_read_without_a_reset() {
    let up = FakeUpstream::start(vec![
        limit_resets_rule(),
        fable_limit_usage_rule(),
        profile_rule(),
    ])
    .await;
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    store_block(
        &h,
        "a",
        r#"{"cooldown_until":null,"eligible":false,"grants":[],"ineligible_reason":"tier"}"#,
        NOW,
    );
    let paused = start_polling(&h, &up).await;

    step_to(&h, &up, &paused, secs(302)).await;
    assert_eq!(
        resets_asked(&up, A),
        0,
        "the set changed, and the stored block holds no reset"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_profile_slot_leaves_the_limit_resets_to_the_next() {
    let up = FakeUpstream::start(vec![limit_resets_rule(), usage_rule(), profile_rule()]).await;
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    store_block(&h, "a", &stored_block(), NOW - 2_850_000);
    let paused = start_polling(&h, &up).await;

    // The last read lies 57.5 minutes back at the 600 s slot and 62.5 at the 900 s slot, which reads the profile.
    step_to(&h, &up, &paused, secs(902)).await;
    assert_eq!(
        (profile_asked(&up, A), resets_asked(&up, A)),
        (1, 0),
        "the slot that reads the profile leaves the due read out"
    );
    step_to(&h, &up, &paused, secs(1_202)).await;
    assert_eq!(resets_asked(&up, A), 1, "the next slot sends it");
    up.shutdown().await;
}

/// Reports a response head of the account's `window` at `utilization` through the router's handle, as the relay
/// reports one, at the harness's clock and under the reset the router's stored reading of the window names.
async fn report_head(h: &Harness, id: &str, window: &str, utilization: f64) {
    let samples = resumed(h.router.samples()).await.unwrap();
    let reset = samples
        .iter()
        .find(|(account, _)| account == id)
        .and_then(|(_, sample)| sample.reading(window))
        .and_then(|reading| reading.reset)
        .expect("a slot's poll stored the window under its reset");
    let at = h.clock.load(Ordering::SeqCst);
    let sample = UtilizationSample {
        observed_at: at,
        ..UtilizationSample::default()
    }
    .with(
        window,
        WindowReading {
            utilization,
            reset: Some(reset),
            observed_at: at,
            source: WindowSource::Head,
            plan: None,
            tier: None,
        },
    );
    h.router
        .report(Report::Sample {
            account_id: id.into(),
            sample,
            incarnation: h.store.account(id).unwrap().unwrap().incarnation,
            source_request_id: None,
        })
        .await;
}

#[tokio::test]
async fn a_window_restart_between_two_slots_reads_the_profile_alone_and_the_third_slot_after_it_reads_it_again(
) {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let paused = start_polling(&h, &up).await;

    // The startup step's read counts as the first, so the 300 s slot reads no profile.
    step_to(&h, &up, &paused, secs(302)).await;
    assert_eq!((usage_asked(&up, A), profile_asked(&up, A)), (1, 0));
    // The week falls from the poll's 0.40 under its reset, as a plan change or a used limit reset leaves it.
    report_head(&h, "a", SEVEN_DAY, 0.10).await;
    step_to(&h, &up, &paused, secs(599)).await;
    assert_eq!(
        (usage_asked(&up, A), profile_asked(&up, A)),
        (1, 1),
        "the restart reads the profile alone before the next slot"
    );
    step_to(&h, &up, &paused, secs(902)).await;
    assert_eq!(
        (usage_asked(&up, A), profile_asked(&up, A)),
        (3, 1),
        "the 600 s and 900 s slots read no profile"
    );
    step_to(&h, &up, &paused, secs(1_202)).await;
    assert_eq!(
        (usage_asked(&up, A), profile_asked(&up, A)),
        (4, 2),
        "the third slot after the restart read reads it"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_restart_read_that_finds_no_profile_leaves_the_cadence_as_it_stood() {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    // A failure that arms no hold.
    up.add_credential_rule(A, "/api/oauth/profile", answering(500));
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let paused = start_polling(&h, &up).await;

    step_to(&h, &up, &paused, secs(302)).await;
    report_head(&h, "a", SEVEN_DAY, 0.10).await;
    step_to(&h, &up, &paused, secs(599)).await;
    assert_eq!(
        (usage_asked(&up, A), profile_asked(&up, A)),
        (1, 1),
        "the restart read leaves once"
    );
    step_to(&h, &up, &paused, secs(899)).await;
    assert_eq!(
        (usage_asked(&up, A), profile_asked(&up, A)),
        (2, 1),
        "the 600 s slot reads no profile"
    );
    step_to(&h, &up, &paused, secs(902)).await;
    assert_eq!(
        (usage_asked(&up, A), profile_asked(&up, A)),
        (3, 2),
        "the 900 s slot reads the profile, where the cadence stood before the restart"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_second_window_restart_inside_the_interval_after_a_restart_read_sends_no_second_read() {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let paused = start_polling(&h, &up).await;

    step_to(&h, &up, &paused, secs(302)).await;
    report_head(&h, "a", SEVEN_DAY, 0.10).await;
    step_to(&h, &up, &paused, secs(362)).await;
    assert_eq!(profile_asked(&up, A), 1, "the first restart's read");
    // The Fable week falls from the poll's 0.42 a minute on.
    report_head(&h, "a", SEVEN_DAY_FABLE, 0.10).await;
    step_to(&h, &up, &paused, secs(599)).await;
    assert_eq!(profile_asked(&up, A), 1, "one read for both restarts");
    up.shutdown().await;
}

#[tokio::test]
async fn a_window_restart_while_a_poll_reads_the_profile_waits_for_it_and_its_read_serves_the_restart(
) {
    let up = FakeUpstream::start(vec![usage_rule(), profile_gated()]).await;
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let paused = start_polling(&h, &up).await;

    // The 900 s slot is the third scheduled poll, so it reads the usage and then the profile, which the gate holds.
    step_to(&h, &up, &paused, secs(897)).await;
    assert_eq!((usage_asked(&up, A), profile_asked(&up, A)), (2, 0));
    paused.idle.set(paused.idle.get() + 1);
    step_to(&h, &up, &paused, secs(901)).await;
    assert_eq!(
        (usage_asked(&up, A), profile_asked(&up, A)),
        (3, 1),
        "the slot's profile read is out, held at the gate"
    );
    // Room for a second read held at the gate, so a restart that sends its own read while the slot's is out lets the
    // step settle with the endpoint asked twice.
    paused.idle.set(paused.idle.get() + 1);
    report_head(&h, "a", SEVEN_DAY, 0.10).await;
    step_to(&h, &up, &paused, secs(902)).await;
    assert_eq!(
        profile_asked(&up, A),
        1,
        "the restart waits for the attempt that is out"
    );

    // The held read answers inside the control client's timeout, and its attempt's end serves the restart.
    up.release();
    paused.idle.set(paused.idle.get() - 2);
    resumed(wait_for(Duration::from_secs(30), || {
        (up.open_connections() <= paused.idle.get()).then_some(())
    }))
    .await;
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    step_to(&h, &up, &paused, secs(1_199)).await;
    assert_eq!(
        (usage_asked(&up, A), profile_asked(&up, A)),
        (3, 1),
        "the poll's profile read served the restart"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_restart_read_s_hold_that_ends_ahead_of_the_next_slot_leaves_the_slot_s_poll_at_its_time()
{
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    up.add_credential_rule(A, "/api/oauth/profile", profile_rule().respond);
    up.push_credential_rule(A, "/api/oauth/profile", rate_limited_for("60"));
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let paused = start_polling(&h, &up).await;

    step_to(&h, &up, &paused, secs(302)).await;
    // The restart read's 429 holds the account for a minute, until about 362 s.
    report_head(&h, "a", SEVEN_DAY, 0.10).await;
    step_to(&h, &up, &paused, secs(599)).await;
    assert_eq!(
        (usage_asked(&up, A), profile_asked(&up, A)),
        (1, 1),
        "no usage read before the 600 s slot"
    );
    step_to(&h, &up, &paused, secs(602)).await;
    assert_eq!(usage_asked(&up, A), 2, "the slot's poll at its time");
    up.shutdown().await;
}

#[tokio::test]
async fn a_restart_read_s_hold_that_ends_past_the_next_slot_moves_the_slot_s_poll_to_the_hold_s_end(
) {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    up.add_credential_rule(A, "/api/oauth/profile", profile_rule().respond);
    up.push_credential_rule(A, "/api/oauth/profile", rate_limited_for("447"));
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let paused = start_polling(&h, &up).await;

    step_to(&h, &up, &paused, secs(302)).await;
    // The restart read's 429 holds the account until about 749 s, past the 600 s slot.
    report_head(&h, "a", SEVEN_DAY, 0.10).await;
    step_to(&h, &up, &paused, secs(748)).await;
    assert_eq!(
        (usage_asked(&up, A), profile_asked(&up, A)),
        (1, 1),
        "no usage read inside the hold"
    );
    // The poll at the hold's end is the second scheduled one, so it reads no profile.
    step_to(&h, &up, &paused, secs(752)).await;
    assert_eq!(
        (usage_asked(&up, A), profile_asked(&up, A)),
        (2, 1),
        "the slot's poll at the hold's end"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_re_pair_sends_the_next_slot_a_limit_resets_read() {
    let up = FakeUpstream::start(vec![limit_resets_rule(), usage_rule(), profile_rule()]).await;
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let usage = h
        .usage
        .clone()
        .expect("the harness was built with the poll");
    // A read at `NOW`, which the service remembers and the row holds, so `start_polling` stamps nothing.
    let block = usage
        .limit_resets("a")
        .await
        .unwrap()
        .expect("the client's shape carries the block");
    assert_eq!(block.grants[0].id, "rg-a-1");
    assert_eq!(resets_asked(&up, A), 1);
    let paused = start_polling(&h, &up).await;

    // The pairing as the admin's confirm leaves it: the harness stored generation 1, and the bundle carries the same
    // access token, so one secret counts both reads.
    h.store
        .commit_pairing(
            "a",
            1,
            1,
            Some(1),
            &NewBundle {
                access_token: Secret::new(A),
                refresh_token: Some(Secret::new(&format!("{A}-refresh"))),
                issued_at: NOW,
                expires_at: i64::MAX / 2,
                refresh_expires_at: None,
                scopes: "user:inference".into(),
            },
        )
        .unwrap();
    usage.invalidate("a");
    step_to(&h, &up, &paused, secs(302)).await;
    assert_eq!(
        resets_asked(&up, A),
        2,
        "the first slot after the pairing reads the block it cleared, five minutes into the hour"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_re_pair_between_two_passes_leaves_the_event_rule_at_one_read() {
    let up = FakeUpstream::start(vec![
        limit_resets_rule(),
        fable_limit_usage_rule(),
        profile_rule(),
    ])
    .await;
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let usage = h
        .usage
        .clone()
        .expect("the harness was built with the poll");
    store_block(&h, "a", &stored_block(), NOW);
    let paused = start_polling(&h, &up).await;

    // The 300 s slot finds the Fable week at its limit where the service remembers no set, so the event rule reads.
    step_to(&h, &up, &paused, secs(302)).await;
    assert_eq!(resets_asked(&up, A), 1, "the event rule's read");

    // A disconnect and a re-pair between two passes, as the admin's two routes leave them, each of which drops the
    // service's state. The registry's ids and incarnations stay as they were, so the slot keeps the row it drew, one
    // lifecycle generation behind the stored one.
    h.store.bump_lifecycle("a", StateCause::Operator).unwrap();
    h.store
        .commit_pairing(
            "a",
            1,
            2,
            None,
            &NewBundle {
                access_token: Secret::new(A),
                refresh_token: Some(Secret::new(&format!("{A}-refresh"))),
                issued_at: NOW,
                expires_at: i64::MAX / 2,
                refresh_expires_at: None,
                scopes: "user:inference".into(),
            },
        )
        .unwrap();
    h.store
        .set_state("a", AccountState::Ready, StateCause::Paired)
        .unwrap();
    usage.invalidate("a");

    // The 600 s slot reads the block the pairing cleared, the 900 s slot reads the profile, and the 1 200 s slot finds
    // the Fable week at its limit as the re-pair's read left it.
    step_to(&h, &up, &paused, secs(602)).await;
    assert_eq!(resets_asked(&up, A), 2, "the re-pair's read");
    step_to(&h, &up, &paused, secs(1_202)).await;
    assert_eq!(
        resets_asked(&up, A),
        2,
        "the same set at the next slot that reads no profile"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_refused_claim_sends_one_limit_resets_read() {
    let up = FakeUpstream::start(vec![
        Rule {
            path_prefix: LIMIT_RESETS_TARGET.into(),
            respond: answering(500),
        },
        usage_rule(),
        profile_rule(),
        Rule {
            path_prefix: "/api/organizations/".into(),
            respond: Respond::Json {
                status: 200,
                headers: vec![],
                body: br#"{"result":"not_limited"}"#.to_vec(),
            },
        },
    ])
    .await;
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let usage = h
        .usage
        .clone()
        .expect("the harness was built with the poll");
    // The harness binds no organization id, and a claim's path carries one.
    h.store
        .set_identity(
            "a",
            &Identity {
                account_uuid: "00000000-0000-4000-8000-000000000100".into(),
                email: "a@example.invalid".into(),
                organization_uuid: Some("00000000-0000-4000-8000-0000000000aa".into()),
            },
        )
        .unwrap();
    // Read at `NOW`, so no hour passes inside the test, and `USAGE` reads no window at its limit, so the mark alone
    // can send a block read.
    store_block(&h, "a", &stored_block(), NOW);
    let row = h.store.account("a").unwrap().unwrap();
    let claim = usage.claim_limit_reset(&row, "rg-a-1", None).await.unwrap();
    assert_eq!(claim.outcome, ClaimOutcome::Refused);
    let paused = start_polling(&h, &up).await;

    step_to(&h, &up, &paused, secs(302)).await;
    assert_eq!(
        resets_asked(&up, A),
        1,
        "the slot after the refusal reads the block"
    );
    step_to(&h, &up, &paused, secs(602)).await;
    assert_eq!(
        resets_asked(&up, A),
        1,
        "the read that went out ended the mark, though it failed"
    );
    up.shutdown().await;
}

/// An answer of `status` with an empty object for its body.
fn answering(status: u16) -> Respond {
    Respond::Json {
        status,
        headers: vec![],
        body: b"{}".to_vec(),
    }
}

/// The rows of `kind` the ledger holds, in order: the account, when, and the detail.
async fn rows_of(h: &Harness, kind: &str) -> Vec<(Option<String>, i64, serde_json::Value)> {
    h.db.events(0, i64::MAX, 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == kind)
        .map(|e| {
            let detail = serde_json::from_str(e.detail.as_deref().unwrap()).unwrap();
            (e.account_id, e.at, detail)
        })
        .collect()
}

#[tokio::test]
async fn a_usage_401_lands_reauth_required_under_auth_failed_and_the_poll_stops_asking_the_account()
{
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    up.add_credential_rule(A, "/api/oauth/usage", answering(401));
    let h = harness_with_poll(
        &up,
        &[oauth("a", A, 0), oauth("c", OAUTH_C, 1)],
        (0, None),
        LedgerConfig::default(),
    )
    .await;
    let paused = start_polling(&h, &up).await;

    step_to(&h, &up, &paused, secs(302)).await;
    assert_eq!(usage_asked(&up, A), 1, "the first slot meets the 401");
    let usage = h.usage.as_ref().unwrap();
    assert_eq!(
        (
            usage.poll_status("a").and_then(|s| s.last_error),
            usage
                .latest_for(&h.store.account("a").unwrap().unwrap())
                .and_then(|s| s.error)
        ),
        (
            Some("usage request rejected with status 401".to_string()),
            Some("usage request rejected with status 401".to_string())
        ),
        "the standing and the snapshot keep the text every refused status writes"
    );
    let row = h.store.account("a").unwrap().unwrap();
    assert_eq!(
        (row.state, row.state_cause),
        (AccountState::ReauthRequired, StateCause::AuthFailed),
        "the usage endpoint refused the credential"
    );
    resumed(routing_reads_reauth_required(&h, "a")).await;
    let moved = resumed(wait_for_async(Duration::from_secs(5), || async {
        rows_of(&h, "account_state_changed").await.pop()
    }))
    .await;
    assert_eq!(
        (
            moved.0.as_deref(),
            moved.2["from"].as_str(),
            moved.2["to"].as_str(),
            moved.2["cause"].as_str()
        ),
        (
            Some("a"),
            Some("ready"),
            Some("reauth_required"),
            Some("auth_failed")
        )
    );

    let sibling = usage_asked(&up, OAUTH_C);
    step_to(&h, &up, &paused, secs(302 + 3 * 300)).await;
    assert_eq!(
        usage_asked(&up, A),
        1,
        "the marked account is out of the schedule"
    );
    assert!(
        usage_asked(&up, OAUTH_C) >= sibling + 3,
        "its sibling keeps the interval"
    );
    assert!(
        resumed(rows_of(&h, "poll_failed")).await.is_empty(),
        "the re-pair writes no passing fault"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_usage_403_writes_poll_failed_naming_the_usage_endpoint_and_keeps_the_schedule() {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    up.add_credential_rule(A, "/api/oauth/usage", answering(403));
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let (lines, _sink) = capture_tracing();
    let paused = start_polling(&h, &up).await;

    step_to(&h, &up, &paused, secs(602)).await;
    assert_eq!(usage_asked(&up, A), 2, "a fault holds no slot back");
    assert_eq!(
        h.store.account("a").unwrap().unwrap().state,
        AccountState::Ready
    );
    let rows = resumed(wait_for_async(Duration::from_secs(5), || async {
        let rows = rows_of(&h, "poll_failed").await;
        (rows.len() == 2).then_some(rows)
    }))
    .await;
    for (account, _, detail) in &rows {
        assert_eq!(account.as_deref(), Some("a"));
        assert_eq!(
            detail,
            &serde_json::json!({
                "endpoint": "usage",
                "status": 403,
                "error": "usage request rejected with status 403",
                "transport": false,
            }),
            "one row per poll that faulted"
        );
    }
    let warnings = lines
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.contains("endpoint failed") && l.contains("account=a") && l.contains("403"))
        .count();
    assert_eq!(warnings, 2, "one warning per poll that faulted");
    up.shutdown().await;
}

#[tokio::test]
async fn a_profile_500_writes_poll_failed_naming_the_profile_endpoint_and_logs_one_warning() {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    up.add_credential_rule(A, "/api/oauth/profile", answering(500));
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let (lines, _sink) = capture_tracing();
    let row = h.store.account("a").unwrap().unwrap();
    QuotaPoll {
        ledger: h.ledger.clone(),
        router: h.router.clone(),
        store: h.store.clone(),
        tables: Arc::new(PlanTables::shipped()),
        usage: h.usage.clone().unwrap(),
    }
    .refresh_profile(&row)
    .await;
    let rows = wait_for_async(Duration::from_secs(5), || async {
        let rows = rows_of(&h, "poll_failed").await;
        (!rows.is_empty()).then_some(rows)
    })
    .await;
    assert_eq!(
        rows.iter()
            .map(|(account, _, detail)| (account.as_deref(), detail.clone()))
            .collect::<Vec<_>>(),
        [(
            Some("a"),
            serde_json::json!({
                "endpoint": "profile",
                "status": 500,
                "error": "profile request rejected with status 500",
                "transport": false,
            })
        )]
    );
    let warnings = lines
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.contains("endpoint failed") && l.contains("account=a"))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("profile") && warnings[0].contains("500"),
        "the line names the endpoint and the status: {}",
        warnings[0]
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_usage_transport_failure_writes_poll_failed_with_transport_set_and_no_status() {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    up.add_credential_rule(A, "/api/oauth/usage", Respond::CloseAfterRequest);
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let row = h.store.account("a").unwrap().unwrap();
    let answer = QuotaPoll {
        ledger: h.ledger.clone(),
        router: h.router.clone(),
        store: h.store.clone(),
        tables: Arc::new(PlanTables::shipped()),
        usage: h.usage.clone().unwrap(),
    }
    .poll_account(&row)
    .await
    .unwrap();
    let text = answer.error.expect("the poll read nothing");
    assert!(text.starts_with("transport: "), "{text}");
    let rows = wait_for_async(Duration::from_secs(5), || async {
        let rows = rows_of(&h, "poll_failed").await;
        (!rows.is_empty()).then_some(rows)
    })
    .await;
    assert_eq!(
        rows.iter()
            .map(|(account, _, detail)| (account.as_deref(), detail.clone()))
            .collect::<Vec<_>>(),
        [(
            Some("a"),
            serde_json::json!({
                "endpoint": "usage",
                "status": null,
                "error": text,
                "transport": true,
            })
        )],
        "the snapshot's text, no status, and the transport flag"
    );
    up.shutdown().await;
}

/// The profile of an organization whose plan ended, as the live read answered it.
const NO_PLAN_PROFILE: &[u8] = br#"{"account":{"uuid":"00000000-0000-4000-8000-000000000100","email":"a@example.invalid","has_claude_max":false,"has_claude_pro":false},"organization":{"uuid":"o","organization_type":"claude_free","rate_limit_tier":"default_claude_ai","subscription_status":"canceled","billing_type":"none"}}"#;

/// The usage endpoint's 403 for an organization that holds no plan.
const PLAN_REFUSED_BODY: &[u8] = br#"{"type":"error","error":{"type":"permission_error","message":"OAuth authentication is currently not allowed for this organization.","details":{"error_code":"oauth_not_allowed_for_organization","error_visibility":"user_facing"}}}"#;

fn no_plan_profile() -> Respond {
    Respond::Json {
        status: 200,
        headers: vec![],
        body: NO_PLAN_PROFILE.to_vec(),
    }
}

fn plan_refused() -> Respond {
    Respond::Json {
        status: 403,
        headers: vec![("x-should-retry".into(), "false".into())],
        body: PLAN_REFUSED_BODY.to_vec(),
    }
}

/// Stores the `default_claude_max_20x` tier as the account's plan, the tier a profile read before the end left.
fn store_max_20x(store: &Store, id: &str) {
    store
        .set_profile(
            id,
            r#"{"organization":{"organization_type":"claude_max","rate_limit_tier":"default_claude_max_20x"}}"#,
            &SubscriptionRow {
                tier: Some("default_claude_max_20x".into()),
                ..SubscriptionRow::default()
            },
        )
        .unwrap();
}

fn profile_asked(up: &FakeUpstream, secret: &str) -> usize {
    asked(up, "/api/oauth/profile", secret)
}

fn plan_ended_at(h: &Harness, id: &str) -> Option<i64> {
    h.store
        .subscription_term(id)
        .unwrap()
        .unwrap()
        .plan_ended_at
}

#[tokio::test]
async fn a_profile_reporting_no_plan_flags_the_account_and_its_slots_read_the_profile_every_third_poll_and_no_usage(
) {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    up.add_credential_rule(A, "/api/oauth/profile", no_plan_profile());
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    store_max_20x(&h.store, "a");
    h.router.reload().await.unwrap();
    let (lines, _sink) = capture_tracing();
    let row = h.store.account("a").unwrap().unwrap();
    QuotaPoll {
        ledger: h.ledger.clone(),
        router: h.router.clone(),
        store: h.store.clone(),
        tables: Arc::new(PlanTables::shipped()),
        usage: h.usage.clone().unwrap(),
    }
    .refresh_profile(&row)
    .await;
    assert_eq!(plan_ended_at(&h, "a"), Some(NOW), "the read's instant");
    let term = h.store.subscription_term("a").unwrap().unwrap();
    assert_eq!(
        (
            term.plan.as_deref(),
            term.tier.as_deref(),
            h.store.account("a").unwrap().unwrap().state
        ),
        (Some("free"), Some("default_claude_ai"), AccountState::Ready),
        "the account answers the found document's plan and tier and the state stands"
    );
    assert_eq!(
        h.router.snapshot().term("a").and_then(|t| t.plan_ended_at),
        Some(NOW),
        "the router reloads the flag"
    );
    let ended = wait_for_async(Duration::from_secs(5), || async {
        rows_of(&h, "plan_ended").await.pop()
    })
    .await;
    assert_eq!(
        (ended.0.as_deref(), ended.1, ended.2),
        (
            Some("a"),
            NOW,
            serde_json::json!({
                "source": "profile",
                "organization_type": "claude_free",
                "rate_limit_tier": "default_claude_ai",
                "subscription_status": "canceled",
            })
        )
    );
    let warnings = lines
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.contains("account=a") && l.contains("claude_free"))
        .count();
    assert_eq!(
        warnings, 1,
        "one line names the account and the profile's words"
    );
    assert_eq!(
        h.usage
            .as_ref()
            .unwrap()
            .poll_status("a")
            .and_then(|s| s.last_error),
        Some("plan ended".to_string())
    );

    let paused = start_polling(&h, &up).await;
    step_to(&h, &up, &paused, secs(899)).await;
    assert_eq!(
        (usage_asked(&up, A), profile_asked(&up, A)),
        (0, 1),
        "no usage request while the flag stands, and no profile read before the third slot"
    );
    step_to(&h, &up, &paused, secs(902)).await;
    assert_eq!(
        (usage_asked(&up, A), profile_asked(&up, A)),
        (0, 2),
        "the third slot reads the profile alone"
    );
    assert_eq!(
        resumed(rows_of(&h, "plan_ended")).await.len(),
        1,
        "a flag that stands writes no second row"
    );
    up.shutdown().await;
}

/// The no-plan branch stores the document it found before it flags the account, so the flag's reload brings the free
/// plan and the found tier to the routing, and the `plan_ended` row names the found document's words.
#[tokio::test]
async fn a_profile_reporting_no_plan_is_stored_before_the_flag_so_the_reload_reads_the_free_plan() {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    up.add_credential_rule(A, "/api/oauth/profile", no_plan_profile());
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    store_max_20x(&h.store, "a");
    h.router.reload().await.unwrap();
    let row = h.store.account("a").unwrap().unwrap();
    QuotaPoll {
        ledger: h.ledger.clone(),
        router: h.router.clone(),
        store: h.store.clone(),
        tables: Arc::new(PlanTables::shipped()),
        usage: h.usage.clone().unwrap(),
    }
    .refresh_profile(&row)
    .await;
    let stored = h
        .store
        .account_extras_of("a")
        .unwrap()
        .unwrap()
        .profile
        .unwrap();
    let found: serde_json::Value = serde_json::from_slice(NO_PLAN_PROFILE).unwrap();
    assert_eq!(stored, found, "the read stores the document it found");
    let snapshot = h.router.snapshot();
    assert_eq!(
        (
            snapshot.plan("a"),
            snapshot.tier("a"),
            snapshot.term("a").and_then(|t| t.plan_ended_at)
        ),
        (Some("free"), Some("default_claude_ai"), Some(NOW)),
        "the flag's reload reads the stored document beside the flag"
    );
    let ended = wait_for_async(Duration::from_secs(5), || async {
        rows_of(&h, "plan_ended").await.pop()
    })
    .await;
    assert_eq!(
        ended.2,
        serde_json::json!({
            "source": "profile",
            "organization_type": "claude_free",
            "rate_limit_tier": "default_claude_ai",
            "subscription_status": "canceled",
        }),
        "the row names the found document's words"
    );
    up.shutdown().await;
}

/// Under a flag that already stands, a read whose found document moves the stored plan and tier reloads the router, so
/// the snapshot reads what the account view answers, and a read that repeats the stored document reloads nothing.
#[tokio::test]
async fn under_a_standing_flag_a_read_that_moves_the_stored_term_reloads_the_router_and_a_repeat_does_not(
) {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    up.add_credential_rule(A, "/api/oauth/profile", no_plan_profile());
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    store_max_20x(&h.store, "a");
    assert!(h.store.set_plan_ended("a", NOW - 60_000).unwrap());
    h.router.reload().await.unwrap();
    let row = h.store.account("a").unwrap().unwrap();
    let poll = QuotaPoll {
        ledger: h.ledger.clone(),
        router: h.router.clone(),
        store: h.store.clone(),
        tables: Arc::new(PlanTables::shipped()),
        usage: h.usage.clone().unwrap(),
    };
    let read = || poll.refresh_profile(&row);
    read().await;
    let moved = h.router.snapshot();
    assert_eq!(
        (moved.plan("a"), moved.tier("a")),
        (Some("free"), Some("default_claude_ai")),
        "the read that moved the stored term reloads the router"
    );
    read().await;
    assert!(
        Arc::ptr_eq(&moved, &h.router.snapshot()),
        "a read that repeats the stored document reloads nothing"
    );
    assert_eq!(
        plan_ended_at(&h, "a"),
        Some(NOW - 60_000),
        "the flag keeps its instant"
    );
    up.shutdown().await;
}

/// The warn case's subscription cost model: `default_claude_max_20x` holds a capacity row and no other word does.
const ONE_TIER_COSTS: &str = "\
unit: {tier: default_claude_max_20x, window: seven_day}
revisions:
  - effective: 1970-01-01
    capacity:
      default_claude_max_20x: {five_hour: 100, seven_day: 1000}
    peak:
      zone: America/Los_Angeles
      weekdays: true
      from_hour: 5
      to_hour: 11
      capacity_factor: {five_hour: 1, seven_day: 1}
    type_multiplier: {cache_read: 0.1, cache_write_1h: 2, cache_write_5m: 2, input: 1, output: 10}
    model_weight:
      claude-sonnet-5: 1
    model_windows: {}
    overrides: {}
    tier_order: [fable, opus, sonnet, haiku]
    model_tier:
      claude-sonnet-5: sonnet
";

/// A profile answer whose organization is `organization`.
fn organization_profile(organization: &str) -> Respond {
    Respond::Json {
        status: 200,
        headers: vec![],
        body: format!(
            r#"{{"account":{{"uuid":"00000000-0000-4000-8000-000000000100","email":"a@example.invalid"}},"organization":{organization}}}"#
        )
        .into_bytes(),
    }
}

/// A Team document whose tier word the cost model holds no row for logs one warning per account and word: a second
/// read of the word logs nothing, a word the model holds logs nothing, another unheld word logs again, and a
/// usage-based Enterprise seat's document never warns. The account ids are this case's alone, since the set of warned
/// pairs is the process's.
#[tokio::test]
async fn a_team_document_whose_tier_the_cost_model_lacks_warns_once_per_account_and_word() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("subscription_costs.yaml");
    std::fs::write(&path, ONE_TIER_COSTS).unwrap();
    let tables = Arc::new(PlanTables::load(Some(&path)).unwrap());
    let team = |tier: &str| {
        organization_profile(&format!(
            r#"{{"uuid":"o","organization_type":"claude_team","rate_limit_tier":"{tier}"}}"#
        ))
    };
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    for tier in [
        "team_word_unheld",
        "team_word_unheld",
        "default_claude_max_20x",
        "team_word_other",
    ] {
        up.push_credential_rule(A, "/api/oauth/profile", team(tier));
    }
    up.add_credential_rule(
        B,
        "/api/oauth/profile",
        organization_profile(
            r#"{"uuid":"e","organization_type":"claude_enterprise","rate_limit_tier":"default_claude_zero","seat_tier":"enterprise_usage_based"}"#,
        ),
    );
    let h = harness_with_poll(
        &up,
        &[oauth("warned-team", A, 0), oauth("quiet-seat", B, 1)],
        (0, None),
        LedgerConfig::default(),
    )
    .await;
    let (lines, _sink) = capture_tracing();
    let read = |id: &'static str| {
        let (h, tables) = (&h, &tables);
        async move {
            let row = h.store.account(id).unwrap().unwrap();
            QuotaPoll {
                ledger: h.ledger.clone(),
                router: h.router.clone(),
                store: h.store.clone(),
                tables: tables.clone(),
                usage: h.usage.clone().unwrap(),
            }
            .refresh_profile(&row)
            .await;
        }
    };
    let warned = |id: &str| {
        lines
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.contains(&format!("account={id} ")) && l.contains("no capacity row"))
            .cloned()
            .collect::<Vec<_>>()
    };
    read("warned-team").await;
    assert_eq!(
        warned("warned-team").len(),
        1,
        "the first read of an unheld word warns"
    );
    assert!(
        warned("warned-team")[0].contains("rate_limit_tier=team_word_unheld"),
        "the line names the word: {:?}",
        warned("warned-team")
    );
    read("warned-team").await;
    assert_eq!(
        warned("warned-team").len(),
        1,
        "a second read of the same word logs nothing"
    );
    read("warned-team").await;
    assert_eq!(
        warned("warned-team").len(),
        1,
        "a word the cost model holds logs nothing"
    );
    read("warned-team").await;
    let lines_of_team = warned("warned-team");
    assert_eq!(
        lines_of_team.len(),
        2,
        "another unheld word warns again: {lines_of_team:?}"
    );
    assert!(lines_of_team[1].contains("rate_limit_tier=team_word_other"));
    let team_term = h.store.subscription_term("warned-team").unwrap().unwrap();
    assert_eq!(
        (team_term.plan.as_deref(), team_term.tier.as_deref()),
        (Some("team"), Some("team_word_other")),
        "every read stored its document"
    );
    read("quiet-seat").await;
    assert_eq!(
        h.store
            .subscription_term("quiet-seat")
            .unwrap()
            .unwrap()
            .plan
            .as_deref(),
        Some("usage_based"),
        "the seat's document is stored and resolves usage-based"
    );
    assert!(
        warned("quiet-seat").is_empty(),
        "a usage-based Enterprise seat never warns"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_profile_reporting_no_plan_ends_the_open_backoff_run_with_its_row() {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    up.add_credential_rule(A, "/api/oauth/usage", plan_refused());
    up.push_credential_rule(A, "/api/oauth/usage", rate_limited_bare());
    up.add_credential_rule(A, "/api/oauth/profile", no_plan_profile());
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let paused = start_polling(&h, &up).await;

    // The first slot at 300 s meets the 429 after the scheduler set its hook, so the run's start row lands. The slot
    // at the hold's end meets the 403, whose same-poll profile read reports no plan and flags the account: the
    // profile endpoint shares the usage endpoint's hold, so a read inside the hold would flag nothing.
    step_to(&h, &up, &paused, secs(302)).await;
    assert_eq!((usage_asked(&up, A), profile_asked(&up, A)), (1, 0));
    step_to(&h, &up, &paused, secs(425)).await;
    assert_eq!(
        (usage_asked(&up, A), profile_asked(&up, A)),
        (2, 1),
        "the 403 made the profile due in the same poll"
    );
    let flagged_at = plan_ended_at(&h, "a").expect("the profile read flags the account");
    // The pause hands the run's end to the hook before the flag's row is recorded, so both have landed once it has.
    resumed(wait_for_async(Duration::from_secs(5), || async {
        rows_of(&h, "plan_ended").await.pop()
    }))
    .await;
    let rows = resumed(backoff_rows(&h)).await;
    assert_eq!(
        rows.iter()
            .map(|row| (row.0.as_str(), row.1.as_deref()))
            .collect::<Vec<_>>(),
        [
            ("poll_backoff_started", Some("a")),
            ("poll_backoff_ended", Some("a"))
        ],
        "the pause closes the run the 429 opened"
    );
    let (started, ended) = (&rows[0], &rows[1]);
    assert_eq!(
        (
            ended.2,
            ended.3["attempts"].as_u64(),
            ended.3["duration_ms"].as_i64(),
            ended.3["resumed_at"].as_i64(),
            ended.3["cause"].as_str()
        ),
        (
            flagged_at,
            Some(1),
            Some(flagged_at - started.2),
            Some(flagged_at),
            Some("plan_ended")
        ),
        "the run lasted from its first 429 to the flag's instant, and its end names the pause"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_usage_403_naming_the_refusal_reads_the_profile_in_the_same_poll_and_flags_a_no_plan_account(
) {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    up.add_credential_rule(A, "/api/oauth/usage", plan_refused());
    up.add_credential_rule(A, "/api/oauth/profile", no_plan_profile());
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let row = h.store.account("a").unwrap().unwrap();
    QuotaPoll {
        ledger: h.ledger.clone(),
        router: h.router.clone(),
        store: h.store.clone(),
        tables: Arc::new(PlanTables::shipped()),
        usage: h.usage.clone().unwrap(),
    }
    .poll_account(&row)
    .await
    .unwrap();
    assert_eq!(
        (usage_asked(&up, A), profile_asked(&up, A)),
        (1, 1),
        "the 403 made the profile due in the same poll"
    );
    assert_eq!(plan_ended_at(&h, "a"), Some(NOW));
    let ended = wait_for_async(Duration::from_secs(5), || async {
        rows_of(&h, "plan_ended").await.pop()
    })
    .await;
    assert_eq!(ended.2["source"], "usage_403");
    assert!(
        rows_of(&h, "poll_failed").await.is_empty(),
        "the ended plan explains the 403, so no passing fault is written"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_usage_403_naming_the_refusal_on_an_account_whose_profile_reports_a_plan_writes_poll_failed(
) {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    up.add_credential_rule(A, "/api/oauth/usage", plan_refused());
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let row = h.store.account("a").unwrap().unwrap();
    QuotaPoll {
        ledger: h.ledger.clone(),
        router: h.router.clone(),
        store: h.store.clone(),
        tables: Arc::new(PlanTables::shipped()),
        usage: h.usage.clone().unwrap(),
    }
    .poll_account(&row)
    .await
    .unwrap();
    assert_eq!(profile_asked(&up, A), 1, "the profile was read");
    assert_eq!(plan_ended_at(&h, "a"), None, "a plan stands");
    let rows = wait_for_async(Duration::from_secs(5), || async {
        let rows = rows_of(&h, "poll_failed").await;
        (!rows.is_empty()).then_some(rows)
    })
    .await;
    assert_eq!(
        rows.iter()
            .map(|(_, _, detail)| (detail["endpoint"].clone(), detail["status"].clone()))
            .collect::<Vec<_>>(),
        [(serde_json::json!("usage"), serde_json::json!(403))],
        "the 403 stays a passing fault"
    );
    assert!(rows_of(&h, "plan_ended").await.is_empty());
    up.shutdown().await;
}

#[tokio::test]
async fn a_profile_with_a_plan_clears_the_flag_stores_its_tier_and_the_next_slot_reads_usage() {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    store_max_20x(&h.store, "a");
    assert!(h.store.set_plan_ended("a", NOW - 60_000).unwrap());
    h.router.reload().await.unwrap();
    let row = h.store.account("a").unwrap().unwrap();
    QuotaPoll {
        ledger: h.ledger.clone(),
        router: h.router.clone(),
        store: h.store.clone(),
        tables: Arc::new(PlanTables::shipped()),
        usage: h.usage.clone().unwrap(),
    }
    .refresh_profile(&row)
    .await;
    assert_eq!(plan_ended_at(&h, "a"), None, "the plan is back");
    assert_eq!(
        h.store
            .subscription_term("a")
            .unwrap()
            .unwrap()
            .tier
            .as_deref(),
        Some("default_claude_max_5x"),
        "the tier the profile names is stored"
    );
    assert_eq!(
        h.router.snapshot().term("a").map(|t| t.plan_ended_at),
        Some(None),
        "the router reloads the cleared flag"
    );
    let restored = wait_for_async(Duration::from_secs(5), || async {
        rows_of(&h, "plan_restored").await.pop()
    })
    .await;
    assert_eq!(
        (restored.0.as_deref(), restored.2["source"].as_str()),
        (Some("a"), Some("profile"))
    );
    assert_eq!(restored.2["organization_type"], "claude_max");

    let paused = start_polling(&h, &up).await;
    step_to(&h, &up, &paused, secs(302)).await;
    assert_eq!(
        usage_asked(&up, A),
        1,
        "the usage poll resumes at the account's next slot"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_run_of_usage_429s_flags_nothing_and_writes_no_poll_failed_row() {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    up.add_credential_rule(A, "/api/oauth/usage", rate_limited_for("3600"));
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let usage = h.usage.as_ref().unwrap();
    let row = h.store.account("a").unwrap().unwrap();
    for step in 0..3 {
        h.clock.store(NOW + step * 900_000, Ordering::SeqCst);
        QuotaPoll {
            ledger: h.ledger.clone(),
            router: h.router.clone(),
            store: h.store.clone(),
            tables: Arc::new(PlanTables::shipped()),
            usage: usage.clone(),
        }
        .poll_account(&row)
        .await
        .unwrap();
    }
    assert_eq!(
        usage_asked(&up, A),
        3,
        "each poll past the capped hold asked"
    );
    assert_eq!(
        (
            plan_ended_at(&h, "a"),
            h.store.account("a").unwrap().unwrap().state
        ),
        (None, AccountState::Ready),
        "a 429 marks nothing"
    );
    for kind in ["plan_ended", "poll_failed"] {
        assert!(rows_of(&h, kind).await.is_empty(), "{kind}");
    }
    up.shutdown().await;
}

#[tokio::test]
async fn a_backoff_run_open_at_the_restore_closes_at_the_first_clean_poll() {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    up.add_credential_rule(A, "/api/oauth/usage", usage_rule().respond);
    up.push_credential_rule(A, "/api/oauth/usage", rate_limited_bare());
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let paused = start_polling(&h, &up).await;

    // The first slot at 300 s opens the run, and the case sets the flag on the store, so the run stands open into the
    // restore, as a run a profile 429 opens under the flag does when the next profile read finds the plan.
    step_to(&h, &up, &paused, secs(302)).await;
    assert_eq!(usage_asked(&up, A), 1);
    assert!(h.store.set_plan_ended("a", NOW + 302_000).unwrap());
    step_to(&h, &up, &paused, secs(600)).await;
    assert_eq!(
        usage_asked(&up, A),
        1,
        "the flag holds the slot after the hold"
    );
    let row = h.store.account("a").unwrap().unwrap();
    resumed(
        QuotaPoll {
            ledger: h.ledger.clone(),
            router: h.router.clone(),
            store: h.store.clone(),
            tables: Arc::new(PlanTables::shipped()),
            usage: h.usage.clone().unwrap(),
        }
        .refresh_profile(&row),
    )
    .await;
    assert_eq!(plan_ended_at(&h, "a"), None, "the profile reports the plan");
    // Read now and asserted with the end row, so one run shows both halves of the rule.
    let after_restore = h
        .usage
        .as_ref()
        .unwrap()
        .poll_status("a")
        .map(|s| (s.rate_limited_in_a_row, s.backoff_since.is_some()));

    step_to(&h, &up, &paused, secs(725)).await;
    assert_eq!(usage_asked(&up, A), 2, "the next slot reads the usage");
    let rows = resumed(wait_for_async(Duration::from_secs(5), || async {
        let rows = backoff_rows(&h).await;
        (rows.len() == 2).then_some(rows)
    }))
    .await;
    assert_eq!(
        rows.iter()
            .map(|row| (row.0.as_str(), row.1.as_deref()))
            .collect::<Vec<_>>(),
        [
            ("poll_backoff_started", Some("a")),
            ("poll_backoff_ended", Some("a"))
        ],
        "the run the 429 opened closes at the first clean poll"
    );
    assert_eq!(
        (
            after_restore,
            rows[1].3["attempts"].as_u64(),
            rows[1].3["cause"].as_str()
        ),
        (Some((1, true)), Some(1), Some("clean_poll")),
        "the restore leaves the run's count and start to the clean poll, whose end row counts the run's 429s and \
         names the clean poll"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn the_startup_step_reads_a_flagged_accounts_profile_alone_and_counts_it_paused() {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    up.add_credential_rule(A, "/api/oauth/profile", no_plan_profile());
    let h = harness_with_poll(
        &up,
        &[oauth("a", A, 0), oauth("c", OAUTH_C, 1)],
        (0, None),
        LedgerConfig::default(),
    )
    .await;
    assert!(h.store.set_plan_ended("a", NOW - 60_000).unwrap());
    let sampled = QuotaPoll {
        ledger: h.ledger.clone(),
        router: h.router.clone(),
        store: h.store.clone(),
        tables: Arc::new(PlanTables::shipped()),
        usage: h.usage.clone().unwrap(),
    }
    .sample_all(Duration::from_secs(2))
    .await;
    assert_eq!(
        sampled,
        Sampled {
            reported: 1,
            failed: 0,
            pending: 0,
            paused: 1
        },
        "the flagged account counts as paused beside the one reported"
    );
    wait_for(Duration::from_secs(5), || {
        (profile_asked(&up, A) == 1).then_some(())
    })
    .await;
    assert_eq!(
        usage_asked(&up, A),
        0,
        "no usage request for the flagged account"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_poll_whose_403_read_the_profile_reads_it_no_second_time() {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    up.add_credential_rule(A, "/api/oauth/usage", plan_refused());
    let h = harness_with_poll(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    // The startup step reads every account's profile, which is due as the 403 makes it due.
    let sampled = QuotaPoll {
        ledger: h.ledger.clone(),
        router: h.router.clone(),
        store: h.store.clone(),
        tables: Arc::new(PlanTables::shipped()),
        usage: h.usage.clone().unwrap(),
    }
    .sample_all(Duration::from_secs(2))
    .await;
    assert_eq!(sampled.failed, 1);
    wait_for_async(Duration::from_secs(5), || async {
        rows_of(&h, "poll_failed").await.pop()
    })
    .await;
    stays_for(Duration::from_millis(500), || profile_asked(&up, A) == 1).await;
    up.shutdown().await;
}

#[test]
fn the_hourly_window_samples_are_each_window_s_last_sample_of_its_hour() {
    let db = LedgerDb::open_in_memory().unwrap();
    let sample_of = |account: &str, bucket: i64, utilization: f64| QuotaSampleRecord {
        account_id: account.into(),
        bucket,
        window: FIVE_HOUR.into(),
        utilization: Some(utilization),
        reset_at: Some(bucket * 1_000 + 3_600_000),
        source: SampleSource::Poll,
        observed_at: bucket * 1_000 + 17_000,
    };
    let sample = |bucket: i64, utilization: f64| sample_of("a", bucket, utilization);
    // Three samples in the hour from 3,600 s written out of order, one in the hour after, and one before the
    // epoch, whose hour floors below it rather than truncating toward zero. A second account samples the same two
    // hours at other buckets, so a read that forgot to scope its newest bucket to the account would drop one side.
    db.insert_samples(
        &[
            sample(3_900, 0.1),
            sample(4_500, 0.3),
            sample(4_200, 0.2),
            sample(7_500, 0.4),
            sample(-2_700, 0.05),
            sample_of("b", 4_400, 0.9),
            sample_of("b", 4_000, 0.8),
            sample_of("b", 7_300, 0.7),
        ],
        None,
    )
    .unwrap();
    let rows = db
        .read_blocking(|c| hourly_window_samples(c, -3_600, 10_800))
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| (r.account_id.as_str(), r.hour, r.bucket))
            .collect::<Vec<_>>(),
        vec![
            ("a", -3_600, -2_700),
            ("a", 3_600, 4_500),
            ("a", 7_200, 7_500),
            ("b", 3_600, 4_400),
            ("b", 7_200, 7_300),
        ],
        "per account the newest of its hour, the negative bucket under the hour below it, ordered by account"
    );
    assert_eq!(
        rows[1],
        HourlyWindowSample {
            account_id: "a".into(),
            hour: 3_600,
            window: FIVE_HOUR.into(),
            bucket: 4_500,
            observed_at: 4_517_000,
            utilization: Some(0.3),
            reset_at: Some(8_100_000),
            utilization_max: Some(0.3),
        },
        "the row carries the sample's own instant beside its bucket"
    );
    let bounded = db
        .read_blocking(|c| hourly_window_samples(c, 3_600, 7_200))
        .unwrap();
    assert_eq!(
        bounded.iter().map(|r| r.bucket).collect::<Vec<_>>(),
        vec![4_500, 4_400],
        "the bounds are hours in seconds, the upper one exclusive"
    );
    let unaligned = db
        .read_blocking(|c| hourly_window_samples(c, 3_600, 4_450))
        .unwrap();
    assert_eq!(
        unaligned
            .iter()
            .map(|r| (r.account_id.as_str(), r.bucket))
            .collect::<Vec<_>>(),
        vec![("b", 4_400)],
        "a bound inside an hour ranks the whole hour: `a`'s 4,500 past it outranks its 3,900 and 4,200 inside it"
    );
}

#[test]
fn latest_quota_answers_each_windows_newest_sample_of_the_day_and_an_account_whose_samples_are_older_is_absent(
) {
    let db = LedgerDb::open_in_memory().unwrap();
    let sample = |account: &str, window: &str, bucket: i64, utilization: f64| QuotaSampleRecord {
        account_id: account.into(),
        bucket,
        window: window.into(),
        utilization: Some(utilization),
        reset_at: Some(bucket * 1_000 + 3_600_000),
        source: SampleSource::Poll,
        observed_at: bucket * 1_000 + 17_000,
    };
    // `a` has two windows, the five-hour one sampled twice in the day, `b` one sample older than the day, and `c` one
    // sample: the table holds one row per bucket and window, so the read answers one row per window it holds.
    db.insert_samples(
        &[
            sample("a", FIVE_HOUR, 90_000, 0.1),
            sample("a", FIVE_HOUR, 90_300, 0.2),
            sample("a", SEVEN_DAY, 90_000, 0.5),
            sample("b", FIVE_HOUR, 1_000, 0.9),
            sample("c", FIVE_HOUR, 90_000, 0.3),
        ],
        None,
    )
    .unwrap();
    let rows = db.read_blocking(|c| latest_quota(c, 100_000_000)).unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| (r.account_id.as_str(), r.window.as_str(), r.bucket))
            .collect::<Vec<_>>(),
        [
            ("a", FIVE_HOUR, 90_300),
            ("a", SEVEN_DAY, 90_000),
            ("c", FIVE_HOUR, 90_000)
        ]
    );
}

#[test]
fn the_lead_over_pace_reads_the_sample_against_its_window_s_elapsed_share() {
    let week = 7 * 24 * 3_600_000;
    let at = 1_700_000_000_000;
    let ahead = lead_over_pace("seven_day", 0.5, Some(at + week * 9 / 10), None, at);
    assert!(
        (ahead - 0.4).abs() < 1e-9,
        "a tenth of the week elapsed by the sample's instant: {ahead}"
    );
    assert_eq!(
        lead_over_pace("five_hour", 0.7, None, None, at),
        0.7,
        "without a reset nothing has elapsed"
    );
    let past = lead_over_pace("five_hour", 0.7, Some(at - 1), None, at);
    assert!(
        (past - (0.7 - 1.0)).abs() < 1e-9,
        "a reset in the past is the whole window elapsed: {past}"
    );
    let fable = lead_over_pace("seven_day_fable", 0.2, Some(at + week / 2), None, at);
    assert!(
        (fable - (0.2 - 0.5)).abs() < 1e-9,
        "the model week is a seven_day window a week long: {fable}"
    );
    let codename = lead_over_pace("cinder_cove", 0.2, Some(at + week / 2), None, at);
    assert!(
        (codename + 0.3).abs() < 1e-9,
        "a week under a name outside seven_day is a week long as well: {codename}"
    );
    let half = lead_over_pace("five_hour", 0.6, Some(at + 9_000_000), None, at);
    assert!(
        (half - 0.1).abs() < 1e-9,
        "a five-hour window half way to its reset: {half}"
    );
}

#[test]
fn the_lead_over_pace_reads_the_steeper_pace_to_an_end_inside_the_window() {
    let week = 7 * 24 * 3_600_000;
    let at = 1_700_000_000_000;
    let reset = Some(at + week * 9 / 10);
    let to_end = lead_over_pace("seven_day", 0.5, reset, Some(at + week / 10), at);
    assert!(
        to_end.abs() < 1e-9,
        "half the span to an end a tenth of the week out has elapsed: {to_end}"
    );
    assert_eq!(
        lead_over_pace("seven_day", 0.5, reset, Some(at + week), at),
        lead_over_pace("seven_day", 0.5, reset, None, at),
        "an end past the reset reads the reset's pace"
    );
}

/// A poll of `body` for the seat at `fetched_at`, as the usage service hands it to the sampler.
fn seat_poll(body: &[u8], fetched_at: i64) -> phoebis_oauth::UsageSnapshot {
    let phoebis_oauth::ParsedUsage {
        windows,
        fable_window,
        extra_usage,
        spend,
        raw_keys,
        fable_week_share,
    } = phoebis_oauth::parse_usage(body).unwrap();
    phoebis_oauth::UsageSnapshot {
        account_id: "seat".into(),
        identity_generation: 1,
        fetched_at,
        windows,
        fable_window,
        extra_usage,
        raw_keys,
        error: None,
        spend,
        fable_week_share,
    }
}

#[tokio::test]
async fn a_polls_extra_usage_row_stores_the_share_as_a_fraction_and_the_spend_objects_figures_alone(
) {
    // The amounts beside the object are a minor unit apart from it, so each column says which object it came from.
    // The share column is the body's declared utilization, read and never computed, so 41.25 against the object's
    // rounded 41 tells the two columns apart.
    let with_object = br#"{"extra_usage":{"is_enabled":true,"monthly_limit":250001,"used_credits":102501.0,"utilization":41.25,"currency":"USD","decimal_places":2},"spend":{"used":{"amount_minor":102500,"currency":"USD","exponent":2},"limit":{"amount_minor":250000,"currency":"USD","exponent":2},"percent":41}}"#;
    let amounts_alone = br#"{"extra_usage":{"is_enabled":true,"monthly_limit":250000,"used_credits":102500.0,"utilization":41.0,"currency":"USD","decimal_places":2}}"#;
    let db = Arc::new(LedgerDb::open_in_memory().unwrap());
    for (body, at) in [(&with_object[..], NOW), (&amounts_alone[..], NOW + 300_000)] {
        let (quota, extra) = phoebis_ledger::samples_from_poll(&seat_poll(body, at));
        db.insert_samples(&quota, extra.as_ref()).unwrap();
    }
    let rows = db.extra_usage_samples("seat", 0, i64::MAX).await.unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| (
                r.utilization,
                r.monthly_limit,
                r.used_credits,
                r.spend_limit_minor,
                r.spend_used_minor,
                r.spend_percent,
                r.spend_currency.as_deref(),
                r.spend_exponent,
            ))
            .collect::<Vec<_>>(),
        [
            (
                Some(0.4125),
                Some(250_001),
                Some(102_501),
                Some(250_000),
                Some(102_500),
                Some(41.0),
                Some("USD"),
                Some(2)
            ),
            (
                Some(0.41),
                Some(250_000),
                Some(102_500),
                None,
                None,
                None,
                None,
                None
            ),
        ],
        "the share reads as the head rows read it, and the spend columns carry the body's object or nothing"
    );
}

/// The bus events of one request on `account` whose answer's head carries `headers`, the selection classing the
/// account as `usage_based`.
fn headed(
    id: &str,
    account: &str,
    usage_based: bool,
    headers: &[(&str, &str)],
    at: i64,
) -> Vec<phoebis_bus::Event> {
    use phoebis_bus::{AccountSelected, AttemptOutcome, Event, Kind, Origin, RequestObserved};
    vec![
        Event::RequestObserved(RequestObserved {
            request_id: id.into(),
            origin: Origin::Client,
            kind: Kind::Inference,
            key_id: None,
            key_generation: 0,
            pool_id: None,
            session_id: None,
            agent_id: None,
            parent_agent_id: None,
            owner_id: None,
            model: "claude-sonnet-5".into(),
            canonical_model: "claude-sonnet-5".into(),
            wire_model: "claude-sonnet-5".into(),
            stream: false,
            headers: vec![],
            body: bytes::Bytes::new(),
            body_capture_hours: 0,
            shape: phoebis_bus::RequestShape::default(),
            static_hash: None,
            boundaries: Vec::new().into(),
            marked: Vec::new().into(),
            client: phoebis_bus::ClientFacts::default(),
            auto_mode: None,
            fork: phoebis_bus::Fork::default(),
            presented_retired: false,
            rotation_at: None,
            guardrail_id: None,
            guardrails: Vec::new(),
            received_at: at,
            fallback_digest: None,
            cache_marker: false,
        }),
        Event::AccountSelected(AccountSelected {
            request_id: id.into(),
            attempt: 1,
            account_id: account.into(),
            account_kind: "oauth".into(),
            usage_based,
            pin_epoch: None,
            started_at: at,
            cache_ttl_ms: None,
            selection: None,
            previous_input_tokens: None,
        }),
        Event::ResponseHead(phoebis_bus::ResponseHead {
            request_id: id.into(),
            attempt: 1,
            status: 200,
            headers: headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
            first_event: None,
            outcome: AttemptOutcome::Succeeded,
            cooldown: None,
            written_at: at,
            observed_at: at,
        }),
    ]
}

#[tokio::test]
async fn a_usage_based_attempts_head_row_carries_the_share_and_the_reset_with_no_in_use_flag_and_a_plan_attempts_keeps_it(
) {
    let reset = "2592000";
    let full = [
        ("anthropic-ratelimit-unified-status", "allowed"),
        ("anthropic-ratelimit-unified-overage-in-use", "true"),
        ("anthropic-ratelimit-unified-overage-utilization", "0.4"),
        ("anthropic-ratelimit-unified-overage-reset", reset),
    ];
    let reset_only = [
        ("anthropic-ratelimit-unified-status", "allowed"),
        ("anthropic-ratelimit-unified-overage-reset", reset),
    ];
    let mut assembler = phoebis_ledger::Assembler::new(Arc::new(|| NOW));
    let db = Arc::new(LedgerDb::open_in_memory().unwrap());
    for events in [
        headed("r-seat", "seat", true, &full, NOW),
        headed("r-reset", "seat-reset", true, &reset_only, NOW),
        headed("r-plan", "plan", false, &full, NOW),
    ] {
        for event in events {
            for record in assembler.observe(event) {
                if let phoebis_ledger::Record::Samples { quota, extra, poll } = record {
                    assert!(
                        !poll,
                        "a head carries some windows only, so its samples remove no gauge series"
                    );
                    db.insert_samples(&quota, extra.as_ref()).unwrap();
                }
            }
        }
    }
    let row = |account: &'static str| {
        let db = db.clone();
        async move {
            db.extra_usage_samples(account, 0, i64::MAX)
                .await
                .unwrap()
                .into_iter()
                .map(|r| {
                    (
                        r.in_use,
                        r.utilization,
                        r.overage_reset,
                        r.source.as_str().to_string(),
                    )
                })
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(
        row("seat").await,
        [(None, Some(0.4), Some(2_592_000_000), "head".to_string())],
        "the seat's head is no overage, so its row carries the share and the reset with no in-use flag"
    );
    assert_eq!(
        row("seat-reset").await,
        [(None, None, Some(2_592_000_000), "head".to_string())],
        "a seat's head that names the reset alone still writes the row"
    );
    assert_eq!(
        row("plan").await,
        [(
            Some(true),
            Some(0.4),
            Some(2_592_000_000),
            "head".to_string()
        )],
        "a plan attempt's head row keeps the head's flag"
    );
}

/// An extra-usage row of account `a` in `bucket` from `source`, with the amounts on a poll row alone.
fn extra_row(
    bucket: i64,
    source: SampleSource,
    utilization: f64,
) -> phoebis_ledger::ExtraUsageSampleRecord {
    let polled = source == SampleSource::Poll;
    phoebis_ledger::ExtraUsageSampleRecord {
        account_id: "a".into(),
        bucket,
        is_enabled: true,
        monthly_limit: polled.then_some(5_000),
        used_credits: polled.then_some(1_234),
        utilization: Some(utilization),
        currency: polled.then(|| "USD".into()),
        overage_reset: (!polled).then_some(bucket * 1_000 + 3_600_000),
        source,
        ..Default::default()
    }
}

#[tokio::test]
async fn the_polled_read_answers_the_poll_row_under_a_newer_head_row_while_the_latest_read_answers_the_head(
) {
    let now = NOW + 900_000;
    let poll = extra_row(phoebis_ledger::bucket_of(NOW), SampleSource::Poll, 0.25);
    let head = extra_row(
        phoebis_ledger::bucket_of(NOW + 600_000),
        SampleSource::Head,
        0.4,
    );
    let db = Arc::new(LedgerDb::open_in_memory().unwrap());
    db.insert_samples(&[], Some(&poll)).unwrap();
    db.insert_samples(&[], Some(&head)).unwrap();
    let (latest, polled) = db
        .read_blocking(move |c| {
            Ok((
                phoebis_ledger::latest_extra_usage(c, now)?,
                phoebis_ledger::latest_extra_usage_polled(c, now)?,
            ))
        })
        .unwrap();
    assert_eq!(latest, [head], "the newest row of any source is the head's");
    assert_eq!(
        polled,
        [poll],
        "the newest polled row still carries the amounts under the head's"
    );
}

#[tokio::test]
async fn the_polled_read_answers_the_poll_row_of_a_bucket_a_head_row_shares_in_either_order() {
    let now = NOW + 600_000;
    let bucket = phoebis_ledger::bucket_of(now);
    let head = extra_row(bucket, SampleSource::Head, 0.4);
    let poll = extra_row(bucket, SampleSource::Poll, 0.25);
    for (first, second, order) in [
        (&head, &poll, "the head first"),
        (&poll, &head, "the poll first"),
    ] {
        let db = Arc::new(LedgerDb::open_in_memory().unwrap());
        db.insert_samples(&[], Some(first)).unwrap();
        db.insert_samples(&[], Some(second)).unwrap();
        let polled = db
            .read_blocking(move |c| phoebis_ledger::latest_extra_usage_polled(c, now))
            .unwrap();
        assert_eq!(
            polled,
            std::slice::from_ref(&poll),
            "{order}: the bucket's poll row answers the polled read"
        );
    }
}

#[tokio::test]
async fn the_share_read_answers_the_newest_row_that_carries_a_share_under_a_newer_head_naming_the_reset_alone(
) {
    let now = NOW + 900_000;
    let poll = extra_row(phoebis_ledger::bucket_of(NOW), SampleSource::Poll, 0.25);
    let head = extra_row(
        phoebis_ledger::bucket_of(NOW + 300_000),
        SampleSource::Head,
        0.4,
    );
    let reset_only = phoebis_ledger::ExtraUsageSampleRecord {
        utilization: None,
        ..extra_row(
            phoebis_ledger::bucket_of(NOW + 600_000),
            SampleSource::Head,
            0.0,
        )
    };
    let db = Arc::new(LedgerDb::open_in_memory().unwrap());
    for row in [&poll, &head, &reset_only] {
        db.insert_samples(&[], Some(row)).unwrap();
    }
    let (latest, share) = db
        .read_blocking(move |c| {
            Ok((
                phoebis_ledger::latest_extra_usage(c, now)?,
                phoebis_ledger::latest_extra_usage_share(c, now)?,
            ))
        })
        .unwrap();
    assert_eq!(
        latest,
        [reset_only],
        "the reset-only head is the newest row"
    );
    assert_eq!(
        share,
        [head],
        "the newest row that carries a share is the head before it, not the older poll"
    );
}

#[tokio::test]
async fn a_head_naming_the_reset_alone_keeps_the_share_an_earlier_head_wrote_in_its_bucket() {
    let now = NOW + 900_000;
    let bucket = phoebis_ledger::bucket_of(NOW + 300_000);
    let poll = extra_row(phoebis_ledger::bucket_of(NOW), SampleSource::Poll, 0.25);
    let head = extra_row(bucket, SampleSource::Head, 1.0);
    let later_reset = head.overage_reset.map(|reset| reset + 3_600_000);
    let reset_only = phoebis_ledger::ExtraUsageSampleRecord {
        utilization: None,
        overage_reset: later_reset,
        ..extra_row(bucket, SampleSource::Head, 0.0)
    };
    let db = Arc::new(LedgerDb::open_in_memory().unwrap());
    for row in [&poll, &head, &reset_only] {
        db.insert_samples(&[], Some(row)).unwrap();
    }
    let share = db
        .read_blocking(move |c| phoebis_ledger::latest_extra_usage_share(c, now))
        .unwrap();
    assert_eq!(
        share,
        [phoebis_ledger::ExtraUsageSampleRecord {
            utilization: Some(1.0),
            overage_reset: later_reset,
            ..head
        }],
        "the bucket's head row keeps its last share under its last reset"
    );
}
