//! `phoebis status`: the proxy listener's readiness probe beside five admin reads through the subcommand client,
//! printed as one screen of at most 80 columns or as one JSON object.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use hyper::Method;
use jiff::tz::TimeZone;
use jiff::Timestamp;
use phoebis_routing::{FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE};
use serde_json::{json, Value};

use crate::admin_client::{AdminClient, ClientError, Common};
use crate::config::ListenerTls;
use crate::{health, tls};

#[derive(clap::Args, Debug)]
pub struct StatusArgs {
    /// Print every pin and every unpooled account rather than the first 20 of each.
    #[arg(long)]
    pub all: bool,
    #[command(flatten)]
    pub common: Common,
}

/// The six reads, each kept with its failure so the screen prints what it could read and names what it could not.
pub struct Reads {
    pub accounts: Result<Value, String>,
    pub attention: Result<Value, String>,
    /// `None` when no file named the proxy listener, so no probe ran.
    pub health: Option<Result<SocketAddr, String>>,
    pub pools: Result<Value, String>,
    pub sessions: Result<Value, String>,
    pub system: Result<Value, String>,
}

/// The three listeners as the file names them, with their schemes.
pub struct Listeners {
    pub admin: String,
    /// `certificate_lines`' answer for the file's pairs, printed under the listeners.
    pub certificates: Vec<String>,
    pub metrics: Option<String>,
    pub proxy: Option<String>,
}

/// One line per listener pair whose leaf needs a look: `tls::expiry_line`'s for a leaf within the expiry warning, or
/// the refusal the loader prints for a certificate file the operator running this cannot read or a leaf past its
/// dates. A running router read its pair at start and keeps serving it until `tls reload`. The file on disk is what
/// the next reload or start meets. `pairs` are `(listener, tls)`.
pub fn certificate_lines(pairs: &[(tls::PublicListener, &Option<ListenerTls>)]) -> Vec<String> {
    pairs
        .iter()
        .filter_map(|(listener, tls)| {
            let pair = tls.as_ref()?;
            match tls::read_leaf(*listener, &pair.cert_file) {
                Ok(leaf) => tls::expiry_line(listener.key(), &pair.cert_file, &leaf),
                Err(error) => Some(error.to_string()),
            }
        })
        .collect()
}

/// The lines the `pins` and `accounts in no pool` blocks print before `and N more`, unless `--all`.
pub const BLOCK_CAP: usize = 20;

/// The screen's width in columns; no line passes it.
pub const WIDTH: usize = 80;

/// The narrowest a label or pool id column gets when the row's other columns leave less; a row that still passes the
/// width is cut at its end.
const LABEL_FLOOR: usize = 12;

/// The fewest columns a pin line keeps for its state after the account. A shedding row keeps the state word with its
/// source, then the age in a cell of its own, so the age prints whole however long the label.
const PIN_STATE_ROOM: usize = 20;

/// The widest an agent id prints in the pins block.
const AGENT_CAP: usize = 10;

/// How many characters of a session id the pins block prints.
const SESSION_CHARS: usize = 12;

/// Where a wrapped line carries on.
const CARRY: &str = "    ";

/// The probe, blocking on a worker, runs beside the five GETs, so a wedged proxy listener does not hold the admin reads
/// back by its budget. The GETs run one after another in the order `operations.md` lists them, each under the client's
/// deadline.
pub async fn read(client: &AdminClient, proxy: Option<(SocketAddr, bool)>) -> Reads {
    // The probe keeps its own three-second budget, so `health` and `status` read the listener the same way, and a
    // shorter `--timeout` shortens it too.
    let budget = health::TIMEOUT.min(client.timeout());
    let probe = async move {
        let (listen, tls) = proxy?;
        Some(
            tokio::task::spawn_blocking(move || {
                health::probe(listen, tls, budget, &health::SystemClock)
            })
            .await
            .unwrap_or_else(|error| Err(format!("the proxy probe did not finish: {error}"))),
        )
    };
    let admin = async {
        let mut reader = Reader {
            client,
            refused: None,
        };
        let system = reader.get("/admin/system").await;
        let attention = reader.get("/admin/attention").await;
        let pools = reader.get("/admin/pools").await;
        let accounts = reader.get("/admin/accounts").await;
        let sessions = reader.get("/admin/sessions").await;
        (system, attention, pools, accounts, sessions)
    };
    let (health, (system, attention, pools, accounts, sessions)) = tokio::join!(probe, admin);
    Reads {
        accounts,
        attention,
        health,
        pools,
        sessions,
        system,
    }
}

/// The five GETs one after another, each under the client's deadline.
struct Reader<'a> {
    client: &'a AdminClient,
    /// The line of a refused secret or a throttled login, once one answers. The admin listener counts every refused
    /// bearer as a failed login and refuses every login after five in a minute, so the reads after a refusal print
    /// its line rather than send the bearer again: one run with a stale secret spends one failure, not five.
    refused: Option<String>,
}

impl Reader<'_> {
    /// One GET's answer, or the one line its section prints in its place.
    async fn get(&mut self, path: &str) -> Result<Value, String> {
        if let Some(line) = &self.refused {
            return Err(line.clone());
        }
        match self.client.expect(Method::GET, path, None).await {
            Ok(answer) => Ok(answer.body),
            Err(error) => {
                let line = error.to_string();
                if matches!(
                    error,
                    ClientError::Refused401 | ClientError::Refused { status: 429, .. }
                ) {
                    self.refused = Some(line.clone());
                }
                Err(line)
            }
        }
    }
}

/// The screen. `binary_version` and `binary_build` stand in for the system answer's where it could not be read. `now`
/// and `zone` are the clock and the local zone, passed in so a test renders a fixed instant.
pub fn render(
    reads: &Reads,
    listeners: &Listeners,
    binary_version: &str,
    binary_build: &str,
    now: Timestamp,
    zone: &TimeZone,
    all: bool,
) -> String {
    let clock = Clock { now, zone };
    let accounts = Accounts::of(&reads.accounts);
    let mut screen = Screen::default();

    let system = reads.system.as_ref().ok();
    let version = system
        .and_then(|s| s["version"].as_str())
        .unwrap_or(binary_version);
    let build = system
        .and_then(|s| s["build"].as_str())
        .unwrap_or(binary_build);
    // The probe's failure is the worst reading, then the system answer's: a proxy that does not answer is down
    // whatever the admin listener says.
    let readiness = match (&reads.health, &reads.system) {
        (Some(Err(reason)), _) => format!("proxy down: {reason}"),
        (_, Err(_)) => "readiness unknown".to_owned(),
        (_, Ok(system)) => {
            let dead: Vec<&str> = system["dead"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            if !dead.is_empty() {
                format!("not ready: {}", dead.join(", "))
            } else if system["ready"] == true {
                "ready".to_owned()
            } else {
                "not ready".to_owned()
            }
        }
    };
    let mut header = format!("phoebis {version}");
    if let Some(seconds) = system.and_then(|s| s["uptime_seconds"].as_u64()) {
        header.push_str(&format!(", up {}", uptime(seconds)));
    }
    header.push_str(&format!(", {readiness}"));
    screen.wrap(&header);
    // A line of its own, since the 40 hex characters beside the version would push `, ready` past the width.
    screen.push(format!("  build {build}"));

    let named: Vec<String> = [
        ("proxy", listeners.proxy.as_deref()),
        ("admin", Some(listeners.admin.as_str())),
        ("metrics", listeners.metrics.as_deref()),
    ]
    .into_iter()
    .filter_map(|(name, url)| url.map(|url| format!("{name} {url}")))
    .collect();
    // Three URLs pass 80 columns at the default addresses, so the line breaks between listeners rather than inside
    // one.
    screen.flow("  ", &named, "  ", "  ");
    for line in &listeners.certificates {
        screen.wrap(&format!("  {line}"));
    }
    if reads.health.is_none() {
        screen.push("  proxy: not probed, no configuration file names the listener");
    }
    match &reads.system {
        Ok(system) => {
            if let Some(figures) = ledger_lines(system) {
                screen.push("  ledger:");
                for figure in figures {
                    screen.push(format!("    {figure}"));
                }
            }
        }
        Err(reason) => screen.wrap(&format!("  system: {reason}")),
    }
    match &reads.attention {
        Ok(attention) => {
            let items: Vec<String> = attention["items"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|item| {
                    let subject = &item["subject"];
                    format!(
                        "{} {} since {}",
                        text(&item["kind"]),
                        named_or(&subject["label"], &subject["id"]),
                        clock.time(item["since"].as_i64())
                    )
                })
                .collect();
            if items.is_empty() {
                screen.push("  attention: none");
            } else {
                screen.push("  attention:");
                for item in items {
                    screen.push(format!("    {item}"));
                }
            }
        }
        Err(reason) => screen.wrap(&format!("  attention: {reason}")),
    }

    screen.blank();
    match &reads.pools {
        Ok(answer) => pools_block(&mut screen, answer, &accounts, &clock),
        Err(reason) => screen.wrap(&format!("pools: {reason}")),
    }

    match &reads.accounts {
        Ok(answer) => {
            let mut unpooled: Vec<(String, String)> = answer["accounts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|account| account["pool"].is_null())
                .map(|account| (accounts.label(text(&account["id"])), state_word(account)))
                .collect();
            if !unpooled.is_empty() {
                unpooled.sort();
                screen.blank();
                screen.push("accounts in no pool");
                let mut rows: Vec<Vec<String>> = unpooled
                    .into_iter()
                    .map(|(label, state)| vec![label, state])
                    .collect();
                fit_column(&mut rows, 0, 2, 2, 0);
                screen.capped(table("  ", rows), all);
            }
        }
        Err(reason) => {
            screen.blank();
            screen.wrap(&format!("accounts: {reason}"));
        }
    }

    screen.blank();
    match &reads.sessions {
        Ok(answer) => pins_block(&mut screen, answer, &accounts, &clock, all),
        Err(reason) => screen.wrap(&format!("sessions: {reason}")),
    }

    let mut out = screen
        .lines
        .iter()
        .map(|line| cut(line.trim_end(), WIDTH))
        .collect::<Vec<_>>()
        .join("\n");
    out.push('\n');
    out
}

/// Each of the ledger's figures that is non-zero or true, then each of the state writer's as `writer: ...`, one per
/// line under `ledger:`, or `None` when all of them read clean.
fn ledger_lines(system: &Value) -> Option<Vec<String>> {
    let ledger = &system["ledger"];
    let writer = &system["router"]["writer"];
    let count = |value: &Value, word: &str| {
        value
            .as_u64()
            .filter(|n| *n > 0)
            .map(|n| format!("{n} {word}"))
    };
    let flag = |value: &Value, word: &str| (*value == true).then(|| word.to_owned());
    let ledger_items = [
        count(&ledger["queued"], "queued"),
        flag(&ledger["behind"], "behind"),
        flag(&ledger["degraded"], "degraded"),
        count(&ledger["parked_priced"], "parked"),
        count(&ledger["retrying"], "retrying"),
    ]
    .into_iter()
    .flatten();
    let writer_items = [
        count(&writer["deferred"], "deferred"),
        flag(&writer["degraded"], "degraded"),
    ]
    .into_iter()
    .flatten()
    .map(|item| format!("writer: {item}"));
    let figures: Vec<String> = ledger_items.chain(writer_items).collect();
    (!figures.is_empty()).then_some(figures)
}

/// `pools`, then one block per pool by id: its status and counts, then one line per member by priority then account
/// id with the account's state, the member's `eligible` word, the windows, and the nearest reset or the cooldown.
fn pools_block(screen: &mut Screen, answer: &Value, accounts: &Accounts, clock: &Clock) {
    let mut pools: Vec<&Value> = answer["pools"].as_array().into_iter().flatten().collect();
    if pools.is_empty() {
        screen.push("pools: none");
        return;
    }
    pools.sort_by(|a, b| text(&a["id"]).cmp(text(&b["id"])));
    screen.push("pools");
    let mut headers: Vec<Vec<String>> = pools
        .iter()
        .map(|pool| {
            let members = pool["members"].as_array().map_or(0, Vec::len) as u64;
            let keys = pool["keys"].as_array().map_or(0, Vec::len) as u64;
            let pins = pool["active_pins"].as_u64().unwrap_or(0);
            let requests = pool["requests_24h"].as_u64().unwrap_or(0);
            vec![
                text(&pool["id"]).to_owned(),
                text(&pool["status"]).to_owned(),
                format!(
                    "{}, {}, {}, {requests} requests/24h",
                    plural(members, "member"),
                    plural(keys, "key"),
                    plural(pins, "pin")
                ),
            ]
        })
        .collect();
    fit_column(&mut headers, 0, 3, 2, 0);
    for (pool, header) in pools.iter().zip(table("  ", headers)) {
        screen.push(header);
        let mut members: Vec<&Value> = pool["members"].as_array().into_iter().flatten().collect();
        members.sort_by(|a, b| {
            a["priority"]
                .as_i64()
                .cmp(&b["priority"].as_i64())
                .then_with(|| text(&a["account_id"]).cmp(text(&b["account_id"])))
        });
        // One Fable column for the block when any member has the window, so the resets line up beneath each other.
        let fable = members.iter().any(|member| {
            accounts
                .get(text(&member["account_id"]))
                .is_some_and(|account| windows_of(account).contains_key(SEVEN_DAY_FABLE))
        });
        // A seat's spend cells are wider than the window cells they stand in for, so its row prints them after its
        // eligibility word and outside the block's columns, which every other member's row keeps as it would without
        // the seat.
        let (mut rows, spend): (Vec<Vec<String>>, Vec<Option<String>>) = members
            .iter()
            .map(|member| {
                let id = text(&member["account_id"]);
                let account = accounts.get(id);
                let mut row = vec![
                    accounts.label(id),
                    account.map_or_else(|| "-".to_owned(), state_word),
                    text(&member["eligible"]).to_owned(),
                ];
                let cells = account.map(|account| windows_cells(account, clock, fable));
                if account.is_some_and(|account| account["spend"].is_object()) {
                    return (row, cells.map(|cells| cells.join("  ")));
                }
                row.extend(cells.into_iter().flatten());
                (row, None)
            })
            .unzip();
        // The reset or the cooldown keeps its width, so a long label is cut before the scope word that says the
        // member is held for one model or the fast lane alone.
        let widest_reset = rows
            .iter()
            .filter_map(|row| row.get(5 + usize::from(fable)))
            .map(|cell| 2 + columns(cell))
            .max()
            .unwrap_or(0);
        fit_column(&mut rows, 0, 5 + usize::from(fable), 4, widest_reset);
        let eligible = widest(&rows, 2);
        for (row, spend) in rows.iter_mut().zip(&spend) {
            if spend.is_some() {
                let pad = eligible.saturating_sub(columns(&row[2]));
                row[2].push_str(&" ".repeat(pad));
            }
        }
        for (line, spend) in table("    ", rows).into_iter().zip(spend) {
            screen.push(match spend {
                Some(cells) => format!("{line}  {cells}"),
                None => line,
            });
        }
    }
}

/// `5h NN%` and `7d NN%` (`-` with no sample), under `fable_column` `fable NN%` when the account has a Fable week and
/// an empty cell when it has none, then `until HH:MM <reason>` under a cooldown (`until HH:MM <scope> <reason>` for a
/// scope narrower than the account) or `resets HH:MM` for the nearest reset to come. A usage-based seat's row, whose
/// `spend` is set, prints `spend NN%` and its amounts in whole major units in the two windows' places, no Fable cell,
/// and its spend window's reset, `resets HH:MM` for a head's and `resets MM-DD est` for the estimate.
fn windows_cells(account: &Value, clock: &Clock, fable_column: bool) -> Vec<String> {
    let windows = windows_of(account);
    let share = |utilization: Option<f64>| {
        utilization.map_or("-".to_owned(), |u| {
            format!("{}%", (u * 100.0).round() as i64)
        })
    };
    let percent = |id: &str| share(windows.get(id).and_then(|w| w["utilization"].as_f64()));
    let spend = &account["spend"];
    let mut cells = if spend.is_object() {
        vec![
            format!("spend {}", share(spend["utilization"].as_f64())),
            spend_amounts(&spend["amounts"]),
        ]
    } else {
        vec![
            format!("5h {}", percent(FIVE_HOUR)),
            format!("7d {}", percent(SEVEN_DAY)),
        ]
    };
    if fable_column && !spend.is_object() {
        cells.push(if windows.contains_key(SEVEN_DAY_FABLE) {
            format!("fable {}", percent(SEVEN_DAY_FABLE))
        } else {
            String::new()
        });
    }
    let now = clock.now.as_millisecond();
    // The account scope holds every request; a model or fast-lane scope holds part of them, so the account's own
    // cooldown names the line, and otherwise the one that lifts last.
    let cooldown = account["cooldowns"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["until"].as_i64().is_some_and(|until| until > now))
        .max_by_key(|c| (c["scope"] == "account", c["until"].as_i64()));
    if let Some(cooldown) = cooldown {
        // A narrower scope names itself ahead of the reason, so a cut line still says the member is held for one
        // model or the fast lane rather than for everything.
        let scope = match text(&cooldown["scope"]) {
            "account" | "" => String::new(),
            scope => format!("{scope} "),
        };
        cells.push(format!(
            "until {} {scope}{}",
            clock.time(cooldown["until"].as_i64()),
            text(&cooldown["reason"])
        ));
    } else if spend.is_object() {
        // The estimate is always 00:00 UTC on the first, so its UTC date names it whole in the room an 80-column row
        // leaves the cell. A head's reset is any instant and reads as the screen's clock prints one.
        cells.push(if spend["reset_source"] == "estimated" {
            format!("resets {} est", utc_date(&spend["resets_at"]))
        } else {
            format!("resets {}", clock.time(spend["resets_at"].as_i64()))
        });
    } else if let Some(reset) = windows
        .values()
        .filter_map(|w| w["resets_at"].as_i64())
        .filter(|at| *at >= now)
        .min()
    {
        cells.push(format!("resets {}", clock.time(Some(reset))));
    }
    cells
}

/// A spend window's `amounts` in whole major units: `$used of $limit`, `$used` alone where the body set no limit, `-`
/// with no amounts, with `$` for `USD` and the currency's code and a space for any other (`EUR 1025 of EUR 2500`).
fn spend_amounts(amounts: &Value) -> String {
    let Some(used) = amounts["used_minor"].as_i64() else {
        return "-".to_owned();
    };
    let scale = 10f64.powi(amounts["exponent"].as_i64().unwrap_or(2) as i32);
    let currency = match text(&amounts["currency"]) {
        "USD" => "$".to_owned(),
        code => format!("{code} "),
    };
    let major = |minor: i64| format!("{currency}{}", (minor as f64 / scale).round() as i64);
    match amounts["limit_minor"].as_i64() {
        Some(limit) => format!("{} of {}", major(used), major(limit)),
        None => major(used),
    }
}

/// An epoch-millisecond instant's UTC date as `MM-DD`, `-` for a value that is none.
fn utc_date(at: &Value) -> String {
    at.as_i64()
        .and_then(|ms| Timestamp::from_millisecond(ms).ok())
        .map_or("-".to_owned(), |at| {
            at.to_zoned(TimeZone::UTC).strftime("%m-%d").to_string()
        })
}

/// The account's windows under the three ids the screen prints, each of which the accounts route answers once.
fn windows_of(account: &Value) -> BTreeMap<&'static str, &Value> {
    account["windows"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|window| {
            let id = text(&window["id"]);
            let id = [FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE]
                .into_iter()
                .find(|known| *known == id)?;
            Some((id, window))
        })
        .collect()
}

/// `pins` as the sessions route answers them: one row per session, the main agent's pin when the session has one.
fn pins_block(screen: &mut Screen, answer: &Value, accounts: &Accounts, clock: &Clock, all: bool) {
    let mut pins: Vec<&Value> = answer["sessions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| row["pinned"] == true)
        .collect();
    if pins.is_empty() {
        screen.push("pins: none");
        return;
    }
    pins.sort_by(|a, b| {
        accounts
            .label(text(&a["account_id"]))
            .cmp(&accounts.label(text(&b["account_id"])))
            .then_with(|| b["last_used_at"].as_i64().cmp(&a["last_used_at"].as_i64()))
    });
    // The route groups by key generation and session and prefers the main agent's pin, so the heading says whose
    // pin a row is.
    screen.push("pins (one per session, the main agent's)");
    let mut rows = vec![["session", "agent", "account", "state"]
        .map(str::to_owned)
        .to_vec()];
    // Beside each row, a shedding row's age and the keepalive words after it: the age is a cell of its own until the
    // labels are fitted, so the cuts reach the labels and never the age.
    let mut shed_ages: Vec<Option<(String, String)>> = vec![None];
    for row in &pins {
        let age = row["last_used_at"].as_i64().map(|at| clock.age(at));
        let mut keepalive = String::new();
        if let Some(keepalive_state) = row["keepalive"]["state"].as_str() {
            keepalive.push_str(&format!(", keepalive {keepalive_state}"));
            if !row["keepalive"]["next_probe_at"].is_null() {
                keepalive.push_str(&format!(
                    " next probe {}",
                    clock.time(row["keepalive"]["next_probe_at"].as_i64())
                ));
            }
        }
        // A stamp written before the row named its reason reads as a round's, the one word its readers had.
        let stamped = row["shed"]["from"].as_str().map(|from| {
            let origin = match row["shed"]["reason"].as_str() {
                None | Some("round") => "shed from",
                Some(_) => "failed over from",
            };
            format!("{origin} {}", accounts.label(from))
        });
        let (state, shed_age) = match (stamped, age) {
            (Some(stamp), Some(age)) => (stamp, Some((format!("{age} ago"), keepalive))),
            (Some(stamp), None) => (format!("{stamp}{keepalive}"), None),
            (None, Some(age)) => (
                format!("pinned {age} ago, {}{keepalive}", text(&row["model"])),
                None,
            ),
            (None, None) => (format!("pinned, {}{keepalive}", text(&row["model"])), None),
        };
        rows.push(vec![
            text(&row["session_id"])
                .chars()
                .take(SESSION_CHARS)
                .collect(),
            cut(named_or(&row["agent_id"], &Value::Null), AGENT_CAP),
            accounts.label(text(&row["account_id"])),
            state,
        ]);
        shed_ages.push(shed_age);
    }
    // The account label is cut first, down to its floor, to leave a shedding row its state word, its source and its
    // age. Past the floor the source is cut as well.
    let shedding = rows
        .iter()
        .zip(&shed_ages)
        .filter_map(|(row, shed_age)| {
            let (age, _) = shed_age.as_ref()?;
            Some(columns(&row[3]) + 2 + columns(age))
        })
        .max()
        .unwrap_or(0);
    fit_column(&mut rows, 2, 3, 2, 2 + PIN_STATE_ROOM.max(shedding));
    let state_at = 2 + (0..3).map(|i| widest(&rows, i) + 2).sum::<usize>();
    for (row, shed_age) in rows.iter_mut().zip(shed_ages) {
        if let Some((age, keepalive)) = shed_age {
            let room = WIDTH.saturating_sub(state_at + 2 + columns(&age));
            row[3] = format!("{}, {age}{keepalive}", cut(&row[3], room));
        }
    }
    let mut lines = table("  ", rows).into_iter();
    if let Some(columns) = lines.next() {
        screen.push(columns);
    }
    screen.capped(lines.collect(), all);
}

/// `render`'s lines before each is cut at `WIDTH`.
#[derive(Default)]
struct Screen {
    lines: Vec<String>,
}

impl Screen {
    fn push(&mut self, line: impl Into<String>) {
        self.lines.push(line.into());
    }

    fn blank(&mut self) {
        self.lines.push(String::new());
    }

    /// `items` after `first`, joined by `sep` and carried onto a line that starts with `indent` where the next item
    /// would pass `WIDTH`; an item is never split, and a carried line ends in the separator's mark.
    fn flow(&mut self, first: &str, items: &[String], sep: &str, indent: &str) {
        let mark = sep.trim_end();
        let mut line = first.to_owned();
        for (i, item) in items.iter().enumerate() {
            let tail = if i + 1 < items.len() { mark.len() } else { 0 };
            if i == 0 {
                line.push_str(item);
            } else if columns(&line) + sep.len() + columns(item) + tail <= WIDTH {
                line.push_str(sep);
                line.push_str(item);
            } else {
                line.push_str(mark);
                self.lines
                    .push(std::mem::replace(&mut line, format!("{indent}{item}")));
            }
        }
        self.lines.push(line);
    }

    /// A line of prose, its leading spaces kept, wrapped at word boundaries onto lines indented by `CARRY`, so a
    /// failure's cause reads whole rather than cut.
    fn wrap(&mut self, line: &str) {
        let words = line.trim_start();
        let lead = &line[..line.len() - words.len()];
        let words: Vec<String> = words.split(' ').map(str::to_owned).collect();
        self.flow(lead, &words, " ", CARRY);
    }

    /// The first `BLOCK_CAP` lines and `and N more`, or every line under `all`.
    fn capped(&mut self, lines: Vec<String>, all: bool) {
        let hidden = if all {
            0
        } else {
            lines.len().saturating_sub(BLOCK_CAP)
        };
        let shown = lines.len() - hidden;
        self.lines.extend(lines.into_iter().take(shown));
        if hidden > 0 {
            self.push(format!("  and {hidden} more"));
        }
    }
}

/// Cuts `column` of `rows` to its widest cell or to the room left beside the other columns before `fixed`, at their
/// widest and two spaces apart, after `indent` and with `reserve` columns kept for what follows, whichever is
/// narrower, and never below `LABEL_FLOOR`. A label then prints whole wherever the row has the room, so two labels
/// that share a long prefix stay apart.
fn fit_column(
    rows: &mut [Vec<String>],
    column: usize,
    fixed: usize,
    indent: usize,
    reserve: usize,
) {
    let others: usize = (0..fixed)
        .filter(|i| *i != column)
        .map(|i| widest(rows, i) + 2)
        .sum();
    let room = WIDTH
        .saturating_sub(indent + others + reserve)
        .max(LABEL_FLOOR);
    for row in rows.iter_mut() {
        if let Some(cell) = row.get_mut(column) {
            *cell = cut(cell, room);
        }
    }
}

/// The widest cell of `column` among `rows`, or 0 when no row reaches it.
fn widest(rows: &[Vec<String>], column: usize) -> usize {
    rows.iter()
        .filter_map(|row| row.get(column))
        .map(|cell| columns(cell))
        .max()
        .unwrap_or(0)
}

/// Rows as columns two spaces apart, each column as wide as its widest cell, every line after `indent`; the last
/// cell of a row is not padded.
fn table(indent: &str, rows: Vec<Vec<String>>) -> Vec<String> {
    let mut widths: Vec<usize> = Vec::new();
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            if widths.len() <= i {
                widths.push(0);
            }
            widths[i] = widths[i].max(columns(cell));
        }
    }
    rows.into_iter()
        .map(|row| {
            let last = row.len().saturating_sub(1);
            let cells: Vec<String> = row
                .into_iter()
                .enumerate()
                .map(|(i, cell)| {
                    if i == last {
                        cell
                    } else {
                        let pad = widths[i] - columns(&cell);
                        format!("{cell}{}", " ".repeat(pad))
                    }
                })
                .collect();
            format!("{indent}{}", cells.join("  "))
        })
        .collect()
}

/// The accounts read by id, so a pool member and a pin print the label the operator gave.
struct Accounts<'a> {
    by_id: BTreeMap<&'a str, &'a Value>,
}

impl<'a> Accounts<'a> {
    fn of(read: &'a Result<Value, String>) -> Accounts<'a> {
        let by_id = read
            .as_ref()
            .ok()
            .and_then(|answer| answer["accounts"].as_array())
            .into_iter()
            .flatten()
            .map(|account| (text(&account["id"]), account))
            .collect();
        Accounts { by_id }
    }

    fn get(&self, id: &str) -> Option<&'a Value> {
        self.by_id.get(id).copied()
    }

    /// The account's label, or its id when the label is empty or the accounts read failed.
    fn label(&self, id: &str) -> String {
        self.get(id)
            .and_then(|account| account["label"].as_str())
            .filter(|label| !label.is_empty())
            .unwrap_or(id)
            .to_owned()
    }
}

/// `paused` for an account routing is turned off on, else the account's `state`, with `plan ended` beside it while the
/// account's plan-ended flag stands, since that account keeps its state.
fn state_word(account: &Value) -> String {
    let word = if account["enabled"] == false {
        "paused"
    } else {
        text(&account["state"])
    };
    if account["plan_ended_at"].is_number() {
        format!("{word}, plan ended")
    } else {
        word.to_owned()
    }
}

/// The clock the screen's times and ages read from, in the zone they print in.
struct Clock<'a> {
    now: Timestamp,
    zone: &'a TimeZone,
}

impl Clock<'_> {
    /// An epoch-millisecond instant as `HH:MM` in the zone, `MM-DD HH:MM` when it falls on another day than now.
    fn time(&self, at: Option<i64>) -> String {
        let Some(at) = at.and_then(|ms| Timestamp::from_millisecond(ms).ok()) else {
            return "-".to_owned();
        };
        let at = at.to_zoned(self.zone.clone());
        let today = self.now.to_zoned(self.zone.clone()).date();
        let format = if at.date() == today {
            "%H:%M"
        } else {
            "%m-%d %H:%M"
        };
        at.strftime(format).to_string()
    }

    /// The time since an epoch-millisecond instant in its largest whole unit: `{n}s`, `{n}m`, `{n}h` or `{n}d`.
    fn age(&self, at: i64) -> String {
        let seconds = (self.now.as_millisecond() - at).max(0) / 1_000;
        match seconds {
            s if s < 60 => format!("{s}s"),
            s if s < 3_600 => format!("{}m", s / 60),
            s if s < 86_400 => format!("{}h", s / 3_600),
            s => format!("{}d", s / 86_400),
        }
    }
}

/// `{N}d {N}h`, `{N}h {N}m` or `{N}m`.
fn uptime(seconds: u64) -> String {
    let (days, hours, minutes) = (
        seconds / 86_400,
        seconds % 86_400 / 3_600,
        seconds % 3_600 / 60,
    );
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

fn plural(n: u64, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// A string member, or the empty string.
fn text(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

/// A non-empty string member, else the fallback's, else `-`.
fn named_or<'a>(value: &'a Value, fallback: &'a Value) -> &'a str {
    value
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| fallback.as_str().filter(|s| !s.is_empty()))
        .unwrap_or("-")
}

fn columns(text: &str) -> usize {
    text.chars().count()
}

/// `text` within `width` columns, ending in `…` when it had to be cut.
fn cut(text: &str, width: usize) -> String {
    if columns(text) <= width {
        return text.to_owned();
    }
    let mut kept: String = text.chars().take(width.saturating_sub(1)).collect();
    kept.push('…');
    kept
}

/// `{"health": {"ready", "addr", "reason"}, "system", "attention", "pools", "accounts", "sessions", "errors": {...}}`;
/// `health` is `null` when no probe ran.
pub fn render_json(reads: &Reads) -> Value {
    let health = match &reads.health {
        None => Value::Null,
        Some(Ok(addr)) => json!({"ready": true, "addr": addr.to_string(), "reason": null}),
        Some(Err(reason)) => json!({"ready": false, "addr": null, "reason": reason}),
    };
    let mut errors = serde_json::Map::new();
    let mut answer = |name: &str, read: &Result<Value, String>| match read {
        Ok(value) => value.clone(),
        Err(reason) => {
            errors.insert(name.to_owned(), json!(reason));
            Value::Null
        }
    };
    let system = answer("system", &reads.system);
    let attention = answer("attention", &reads.attention);
    let pools = answer("pools", &reads.pools);
    let accounts = answer("accounts", &reads.accounts);
    let sessions = answer("sessions", &reads.sessions);
    json!({
        "health": health,
        "system": system,
        "attention": attention,
        "pools": pools,
        "accounts": accounts,
        "sessions": sessions,
        "errors": errors,
    })
}

/// 0 when `system.ready` is true and the probe, where it ran, answered; 1 when the probe ran and failed, `ready` is
/// false, or `system` could not be read. A skipped probe (`health` `None`, no file named the listener) is not a
/// failed one: readiness then reads from the authenticated system answer alone.
pub fn exit_code(reads: &Reads) -> i32 {
    let probe = !matches!(reads.health, Some(Err(_)));
    let ready = reads
        .system
        .as_ref()
        .is_ok_and(|system| system["ready"] == true);
    if probe && ready {
        0
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    /// One captured admin answer, unwrapped from its `{"route", "body"}` envelope.
    fn fixture(name: &str) -> Value {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../web/test/fixtures")
            .join(format!("{name}.json"));
        let wrapped: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        wrapped["body"].clone()
    }

    fn fixture_reads() -> Reads {
        Reads {
            accounts: Ok(fixture("accounts")),
            attention: Ok(fixture("attention")),
            health: Some(Ok("127.0.0.1:8317".parse().unwrap())),
            pools: Ok(fixture("pools")),
            sessions: Ok(fixture("sessions")),
            system: Ok(fixture("system")),
        }
    }

    /// A pair whose leaf's `not_after` is `days` days and an hour from now, so it has `days` whole days left and part
    /// of one whatever the clock reads.
    fn expiring_pair(dir: &Path, days: i64) -> (std::path::PathBuf, std::path::PathBuf) {
        let not_after = Timestamp::now().as_second() + days * 86_400 + 3_600;
        fake_upstream::generate_pair_valid_until(&["localhost"], not_after).write_to(dir, "proxy")
    }

    #[test]
    fn a_certificate_line_names_a_leaf_inside_the_warning_or_a_certificate_the_start_would_refuse()
    {
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = expiring_pair(dir.path(), 400);
        let distant = Some(ListenerTls {
            cert_file: cert,
            key_file: key,
        });
        assert!(certificate_lines(&[
            (tls::PublicListener::Proxy, &distant),
            (tls::PublicListener::Admin, &None)
        ])
        .is_empty());

        let missing = dir.path().join("absent.pem");
        let unreadable = Some(ListenerTls {
            cert_file: missing.clone(),
            key_file: dir.path().join("absent.key"),
        });
        let lines = certificate_lines(&[(tls::PublicListener::Admin, &unreadable)]);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].starts_with(&format!("admin.tls.cert_file {}: ", missing.display())),
            "{lines:?}"
        );
    }

    fn listeners() -> Listeners {
        Listeners {
            admin: "http://127.0.0.1:8081".to_owned(),
            metrics: Some("http://127.0.0.1:9464".to_owned()),
            proxy: Some("https://0.0.0.0:8317".to_owned()),
            certificates: Vec::new(),
        }
    }

    /// The fixtures' own clock, the admin harness's: their `since`, `set_at` and `last_used_at` read it, 2026-10-11
    /// 12:16:40 UTC.
    const NOW_MS: i64 = 1_791_721_000_000;

    fn now() -> Timestamp {
        Timestamp::from_millisecond(NOW_MS).unwrap()
    }

    /// An instant of the clock's own day as the screen prints it in UTC.
    fn hh_mm(at_ms: i64) -> String {
        Timestamp::from_millisecond(at_ms)
            .unwrap()
            .to_zoned(TimeZone::UTC)
            .strftime("%H:%M")
            .to_string()
    }

    /// The lines from the one equal to `heading` up to the next blank line, the heading excluded.
    fn block<'a>(lines: &[&'a str], heading: &str) -> Vec<&'a str> {
        lines
            .iter()
            .skip_while(|line| **line != heading)
            .skip(1)
            .take_while(|line| !line.is_empty())
            .copied()
            .collect()
    }

    /// The fixture's pinned row as `count` pinned sessions, every fifth stamped from `direct` (every tenth by a round,
    /// the others by a failover) and every third with a keepalive, beside the fixture's unpinned rows.
    fn pinned_sessions(count: usize) -> Value {
        let sessions = fixture("sessions")["sessions"].as_array().unwrap().clone();
        let pinned = sessions
            .iter()
            .find(|row| row["pinned"] == true)
            .unwrap()
            .clone();
        let mut rows: Vec<Value> = sessions
            .into_iter()
            .filter(|row| row["pinned"] != true)
            .collect();
        for i in 0..count {
            let mut row = pinned.clone();
            row["session_id"] = json!(format!("s-ext-{i:02}"));
            row["last_used_at"] = json!(NOW_MS - i as i64 * 60_000);
            if i % 5 == 0 {
                row["shed"] = json!({
                    "from": "direct",
                    "at": NOW_MS - 3_600_000,
                    "reason": if i % 10 == 0 { "round" } else { "cooldown" },
                });
            }
            if i % 3 == 0 {
                row["keepalive"]["state"] = json!("idle");
                row["keepalive"]["next_probe_at"] = json!(NOW_MS + 200_000);
            }
            rows.push(row);
        }
        json!({ "sessions": rows })
    }

    #[test]
    fn the_screen_renders_the_captured_answers_in_eighty_columns() {
        // A second pool after `main` in the answer that sorts before it by id, its two members on one priority and
        // out of id order, so the screen's own ordering is what the test reads.
        let mut pools_answer = fixture("pools");
        pools_answer["pools"].as_array_mut().unwrap().push(json!({
            "id": "lab", "status": "degraded", "keys": [], "active_pins": 0, "requests_24h": 0,
            "members": [
                {"account_id": "direct", "priority": 0, "eligible": "eligible"},
                {"account_id": "business", "priority": 0, "eligible": "cooldown"},
            ],
        }));
        let reads = Reads {
            pools: Ok(pools_answer.clone()),
            ..fixture_reads()
        };
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = expiring_pair(dir.path(), 30);
        let tls = Some(ListenerTls {
            cert_file: cert.clone(),
            key_file: key,
        });
        let expiring = Listeners {
            certificates: certificate_lines(&[
                (tls::PublicListener::Proxy, &tls),
                (tls::PublicListener::Admin, &None),
            ]),
            ..listeners()
        };
        let screen = render(
            &reads,
            &expiring,
            "9.9.9",
            "unknown",
            now(),
            &TimeZone::UTC,
            false,
        );
        let lines: Vec<&str> = screen.lines().collect();
        for line in &lines {
            assert!(line.chars().count() <= WIDTH, "{line}");
        }
        let after = lines
            .iter()
            .position(|line| line.starts_with("  metrics "))
            .unwrap()
            + 1;
        assert!(
            lines[after].starts_with(&format!("  proxy.tls.cert_file {} ", cert.display())),
            "the expiry line follows the listeners: {screen}"
        );
        let expiry: Vec<&str> = lines[after..]
            .iter()
            .take_while(|line| !line.starts_with("  ledger") && !line.starts_with("  attention"))
            .map(|line| line.trim())
            .collect();
        assert!(
            expiry.join(" ").contains(" expires in 30 days ("),
            "{screen}"
        );

        let system = fixture("system");
        assert!(
            lines[0].starts_with(&format!(
                "phoebis {}, up ",
                system["version"].as_str().unwrap()
            )),
            "{screen}"
        );
        // The captured system answer is ready and names no dead component.
        assert!(lines[0].ends_with(", ready"), "{screen}");
        assert_eq!(
            lines[1],
            format!("  build {}", system["build"].as_str().unwrap()),
            "the system answer's build leads the binary's: {screen}"
        );
        let ledger = lines.iter().position(|line| *line == "  ledger:");
        assert_eq!(
            ledger.map(|at| lines[at + 1]),
            Some("    1 parked"),
            "{screen}"
        );

        // The captured items in their order, each `since` on the fixtures' clock in UTC.
        assert_eq!(
            block(&lines, "  attention:"),
            [
                "    pool_exhausted Main since 12:16",
                "    pairing_expiring Seat since 10-10 12:16",
                "    limit_reset_ending Reset ending since 10-09 00:00",
                "    ledger_record_parked Main since 12:16",
                "    model_unpriced claude-opus-99 since 12:15",
                "    meter_drift Business since 10:16",
                "    wall_reached Business since 11:56",
                "    destination_spent Business since 11:26",
                "    shed_no_runway Main since 11:21",
                "    slowed_long Business since 06:16",
                "    expired_between_turns Keepalive since 12:16",
            ],
            "{screen}"
        );

        let pools_block = block(&lines, "pools");
        let accounts = fixture("accounts");
        let label_of = |id: &str| {
            accounts["accounts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["id"] == id)
                .map(|a| a["label"].as_str().unwrap().to_owned())
                .unwrap()
        };
        // `lab` and its two members, then `main` and the fixture's three members on priorities 0, 1 and 2, then
        // `retired` and its one member.
        assert_eq!(
            pools_block.len(),
            9,
            "one line per pool and member: {screen}"
        );
        assert!(
            pools_block[0].starts_with("  lab ")
                && pools_block[1].starts_with("    Business ")
                && pools_block[2].starts_with("    Direct ")
                && pools_block[3].starts_with("  main "),
            "the pools by id, a priority's members by account id: {screen}"
        );
        assert!(
            pools_block[0].contains("degraded")
                && pools_block[3].contains("active")
                && pools_block[4].starts_with("    Business ")
                && pools_block[5].starts_with("    Direct ")
                && pools_block[6].starts_with("    Seat "),
            "each pool's header carries its status and each member line its label: {screen}"
        );
        let cooldown_until = fixture("accounts")["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|account| account["id"] == "business")
            .and_then(|account| account["cooldowns"][0]["until"].as_i64())
            .expect("the fixture's Business account carries a cooldown");
        // The windows and the cooldown of `main`'s first member, cut at the width, and the dashes of a member with no
        // sample.
        assert!(
            pools_block[4].contains("5h 62%")
                && pools_block[4].contains("7d 41%")
                && pools_block[4].contains("fable 12%")
                && pools_block[4].contains(&format!("until {} model:", hh_mm(cooldown_until)))
                && pools_block[4].ends_with('…'),
            "{screen}"
        );
        assert!(
            pools_block[5].contains("5h -") && pools_block[5].contains("7d -"),
            "{screen}"
        );

        let pins = block(&lines, "pins (one per session, the main agent's)");
        let sessions = fixture("sessions");
        let pinned: Vec<&Value> = sessions["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["pinned"] == true)
            .collect();
        assert!(!pinned.is_empty());
        assert_eq!(
            pins.len(),
            1 + pinned.len(),
            "a column line, then one per pin: {screen}"
        );
        // The screen sorts its pins by account label, then newest first, so a line pairs with its row by the session
        // id it opens with, cut as the screen cuts it.
        let mut paired = std::collections::BTreeSet::new();
        for line in &pins[1..] {
            let session = line.split_whitespace().next().unwrap();
            let row = pinned
                .iter()
                .find(|row| {
                    text(&row["session_id"])
                        .chars()
                        .take(SESSION_CHARS)
                        .eq(session.chars())
                })
                .unwrap_or_else(|| panic!("no pinned row for {line}"));
            assert!(paired.insert(session), "one line per pin: {screen}");
            assert!(
                line.contains(&label_of(row["account_id"].as_str().unwrap())),
                "{line}"
            );
            // A stamped row reads its stamp's words in the place of `pinned`.
            assert_eq!(line.contains("pinned"), row["shed"].is_null(), "{line}");
            assert_eq!(
                line.contains("shed from") || line.contains("failed over from"),
                !row["shed"].is_null(),
                "{line}"
            );
            assert_eq!(
                line.contains("keepalive"),
                !row["keepalive"]["state"].is_null(),
                "{line}"
            );
        }

        let mut extended = fixture_reads();
        extended.sessions = Ok(pinned_sessions(25));
        let capped = render(
            &extended,
            &listeners(),
            "9.9.9",
            "unknown",
            now(),
            &TimeZone::UTC,
            false,
        );
        let capped_lines: Vec<&str> = capped.lines().collect();
        let capped_pins = block(&capped_lines, "pins (one per session, the main agent's)");
        assert_eq!(capped_pins.len(), 1 + BLOCK_CAP + 1, "{capped}");
        assert_eq!(capped_pins[BLOCK_CAP + 1], "  and 5 more", "{capped}");
        // The newest first under one label: the rows' `last_used_at` falls with their index.
        assert!(capped_pins[1].starts_with("  s-ext-00 "), "{capped}");
        assert!(
            capped_pins[BLOCK_CAP].starts_with("  s-ext-19 "),
            "{capped}"
        );

        let whole = render(
            &extended,
            &listeners(),
            "9.9.9",
            "unknown",
            now(),
            &TimeZone::UTC,
            true,
        );
        let whole_lines: Vec<&str> = whole.lines().collect();
        let whole_pins = block(&whole_lines, "pins (one per session, the main agent's)");
        assert_eq!(whole_pins.len(), 1 + 25, "{whole}");
        assert!(
            !whole_pins.iter().any(|line| line.starts_with("  and ")),
            "{whole}"
        );
        let stamped = |words: &str| {
            whole_pins
                .iter()
                .filter(|line| line.contains(&format!("{words} Direct, ")))
                .count()
        };
        assert_eq!(
            (stamped("shed from"), stamped("failed over from")),
            (3, 2),
            "{whole}"
        );
        // A keepalive's suffix runs past the width on these rows and is cut there; its first words survive.
        assert_eq!(
            whole_pins
                .iter()
                .filter(|line| line.contains(", keepalive idle"))
                .count(),
            9,
            "{whole}"
        );
        let mut short = pinned_sessions(1);
        let row = &mut short["sessions"]
            .as_array_mut()
            .unwrap()
            .last_mut()
            .unwrap();
        row["agent_id"] = Value::Null;
        row["account_id"] = json!("direct");
        row["model"] = json!("m");
        row["shed"] = Value::Null;
        let mut one = fixture_reads();
        one.sessions = Ok(short);
        let one_screen = render(
            &one,
            &listeners(),
            "9.9.9",
            "unknown",
            now(),
            &TimeZone::UTC,
            false,
        );
        let one_lines: Vec<&str> = one_screen.lines().collect();
        assert_eq!(
            block(&one_lines, "pins (one per session, the main agent's)"),
            [
                "  session   agent  account  state".to_string(),
                format!(
                    "  s-ext-00  -      Direct   pinned 0s ago, m, keepalive idle next probe {}",
                    hh_mm(NOW_MS + 200_000)
                )
            ],
            "{one_screen}"
        );
        assert!(
            whole_pins
                .iter()
                .filter(|line| line.contains("pinned"))
                .all(|line| line.contains("claude-sonnet-5")),
            "{whole}"
        );

        let json = render_json(&reads);
        assert_eq!(
            json["health"],
            json!({"ready": true, "addr": "127.0.0.1:8317", "reason": null})
        );
        for name in ["system", "attention", "accounts", "sessions"] {
            assert_eq!(json[name], fixture(name), "{name}");
        }
        assert_eq!(json["pools"], pools_answer);
        assert_eq!(json["errors"], json!({}));
    }

    #[test]
    fn each_attention_item_prints_on_its_own_line_under_the_label_and_an_empty_list_reads_none() {
        let item = |kind: &str, label: &str| {
            json!({
                "kind": kind,
                "subject": {"type": "pool", "id": label, "label": label},
                "since": NOW_MS,
            })
        };
        let two = Reads {
            attention: Ok(json!({"items": [
                item("pool_exhausted", "Main"),
                item("refresh_failed", "alice@example.com"),
            ]})),
            ..fixture_reads()
        };
        let screen = render(
            &two,
            &listeners(),
            "9.9.9",
            "unknown",
            now(),
            &TimeZone::UTC,
            false,
        );
        let lines: Vec<&str> = screen.lines().collect();
        assert_eq!(
            block(&lines, "  attention:"),
            [
                format!("    pool_exhausted Main since {}", hh_mm(NOW_MS)),
                format!(
                    "    refresh_failed alice@example.com since {}",
                    hh_mm(NOW_MS)
                ),
            ],
            "{screen}"
        );

        let none = Reads {
            attention: Ok(json!({"items": []})),
            ..fixture_reads()
        };
        let screen = render(
            &none,
            &listeners(),
            "9.9.9",
            "unknown",
            now(),
            &TimeZone::UTC,
            false,
        );
        let attention: Vec<&str> = screen
            .lines()
            .filter(|line| line.starts_with("  attention"))
            .collect();
        assert_eq!(attention, ["  attention: none"], "{screen}");
    }

    #[test]
    fn each_ledger_figure_prints_on_its_own_line_under_the_label_and_a_clean_ledger_prints_no_block(
    ) {
        let system = |parked: u64, retrying: u64, deferred: u64| {
            json!({
                "version": "9.9.9",
                "ready": true,
                "dead": [],
                "ledger": {
                    "queued": 0,
                    "behind": false,
                    "degraded": false,
                    "parked_priced": parked,
                    "retrying": retrying,
                },
                "router": {"writer": {"deferred": deferred, "degraded": false}},
            })
        };
        let busy = Reads {
            system: Ok(system(2, 1, 3)),
            ..fixture_reads()
        };
        let screen = render(
            &busy,
            &listeners(),
            "9.9.9",
            "unknown",
            now(),
            &TimeZone::UTC,
            false,
        );
        let lines: Vec<&str> = screen.lines().collect();
        let figures: Vec<&str> = lines
            .iter()
            .skip_while(|line| **line != "  ledger:")
            .skip(1)
            .take_while(|line| line.starts_with("    "))
            .copied()
            .collect();
        assert_eq!(
            figures,
            ["    2 parked", "    1 retrying", "    writer: 3 deferred"],
            "{screen}"
        );

        let clean = Reads {
            system: Ok(system(0, 0, 0)),
            ..fixture_reads()
        };
        let screen = render(
            &clean,
            &listeners(),
            "9.9.9",
            "unknown",
            now(),
            &TimeZone::UTC,
            false,
        );
        assert!(
            !screen.lines().any(|line| line.starts_with("  ledger")),
            "{screen}"
        );
    }

    /// The fixture reads with the two accounts' labels replaced.
    fn labelled(business: &str, direct: &str) -> Reads {
        let mut accounts = fixture("accounts");
        for account in accounts["accounts"].as_array_mut().unwrap() {
            let label = if account["id"] == "business" {
                business
            } else {
                direct
            };
            account["label"] = json!(label);
        }
        Reads {
            accounts: Ok(accounts),
            ..fixture_reads()
        }
    }

    #[test]
    fn a_label_column_is_as_wide_as_its_widest_label_where_the_row_has_room() {
        // Two labels that agree on their first twenty columns stay apart on the member lines and the pin line. The
        // member lines carry no reset or cooldown and the pin lines no stamp here, since each keeps its width ahead of
        // the label.
        let one = "engineering-shared-1@corp.io";
        let two = "engineering-shared-2@corp.io";
        let mut roomy = labelled(one, two);
        for account in roomy.accounts.as_mut().unwrap()["accounts"]
            .as_array_mut()
            .unwrap()
        {
            account["cooldowns"] = json!([]);
            for window in account["windows"].as_array_mut().unwrap() {
                window["resets_at"] = Value::Null;
            }
        }
        for row in roomy.sessions.as_mut().unwrap()["sessions"]
            .as_array_mut()
            .unwrap()
        {
            row["shed"] = Value::Null;
        }
        let screen = render(
            &roomy,
            &listeners(),
            "9.9.9",
            "unknown",
            now(),
            &TimeZone::UTC,
            false,
        );
        let lines: Vec<&str> = screen.lines().collect();
        let members = block(&lines, "pools");
        assert!(members[1].starts_with(&format!("    {one}  ")), "{screen}");
        assert!(members[2].starts_with(&format!("    {two}  ")), "{screen}");
        let pins = block(&lines, "pins (one per session, the main agent's)");
        assert!(pins[1].contains(&format!("  {one}  pinned ")), "{screen}");

        // A label past the room the other columns leave still ends in `…`, and the line stays within the width.
        let long = "a-label-far-too-long-for-any-column-of-an-eighty-column-screen";
        let screen = render(
            &labelled(long, two),
            &listeners(),
            "9.9.9",
            "unknown",
            now(),
            &TimeZone::UTC,
            false,
        );
        let lines: Vec<&str> = screen.lines().collect();
        let members = block(&lines, "pools");
        let cut = members[1].trim_start().split("  ").next().unwrap();
        assert!(
            cut.ends_with('…') && long.starts_with(cut.trim_end_matches('…')),
            "{screen}"
        );
        for line in &lines {
            assert!(line.chars().count() <= WIDTH, "{line}");
        }
    }

    /// The first member line for the fixture reads with the business account's cooldowns replaced.
    fn member_line_with(cooldowns: Value) -> String {
        let mut accounts = fixture("accounts");
        for account in accounts["accounts"].as_array_mut().unwrap() {
            if account["id"] == "business" {
                account["cooldowns"] = cooldowns.clone();
            }
        }
        let reads = Reads {
            accounts: Ok(accounts),
            ..fixture_reads()
        };
        let screen = render(
            &reads,
            &listeners(),
            "9.9.9",
            "unknown",
            now(),
            &TimeZone::UTC,
            false,
        );
        let lines: Vec<&str> = screen.lines().collect();
        block(&lines, "pools")[1].to_owned()
    }

    #[test]
    fn a_cooldown_narrower_than_the_account_names_its_scope() {
        let cooldown = |scope: &str, until: i64, reason: &str| json!({"scope": scope, "until": until, "reason": reason, "window": null, "set_at": NOW_MS});
        let (later, sooner) = (NOW_MS + 1_800_000, NOW_MS + 800_000);
        // The fixture's own cooldown holds one model; its scope comes before the reason, so the cut keeps it.
        let model = member_line_with(json!([cooldown(
            "model:claude-haiku-4-5-20251001",
            later,
            "model_rejected"
        )]));
        assert!(
            model.contains(&format!("  until {} model:", hh_mm(later))),
            "{model}"
        );
        let fast = member_line_with(json!([cooldown("fast", later, "rate_limit")]));
        assert!(
            fast.contains(&format!("  until {} fast rate", hh_mm(later))),
            "{fast}"
        );
        // The account scope holds every request, names the line ahead of a narrower one, and reads without a scope.
        let account = member_line_with(json!([
            cooldown("model:claude-haiku-4-5-20251001", later, "model_rejected"),
            cooldown("account", sooner, "rate_limit"),
        ]));
        assert!(
            account.ends_with(&format!("  until {} rate_limit", hh_mm(sooner))),
            "{account}"
        );
    }

    /// Thirty columns, past what a pin line or a member line leaves a label beside the cells it keeps whole.
    const LONG_LABEL: &str = "operations-shared-07@corp.test";

    #[test]
    fn a_shedding_pin_prints_its_age_whole_however_long_the_labels() {
        assert_eq!(LONG_LABEL.chars().count(), 30);
        let twelve_seconds_ago = NOW_MS - 12_000;
        let pin = |session: &str, agent: Value, account: &str, from: &str, reason: &str| {
            json!({
                "session_id": session, "agent_id": agent, "account_id": account,
                "shed": {"from": from, "at": twelve_seconds_ago, "reason": reason}, "pinned": true,
                "model": "claude-sonnet-5", "last_used_at": twelve_seconds_ago,
                "keepalive": {"state": null, "next_probe_at": null},
            })
        };
        let reads = Reads {
            sessions: Ok(json!({"sessions": [
                pin("7b21aa90f3d1aaaa", json!("a1"), "business", "direct", "round"),
                pin("3f9a0c12e4b7bbbb", Value::Null, "direct", "business", "mark"),
            ]})),
            ..labelled(LONG_LABEL, "Direct")
        };
        let screen = render(
            &reads,
            &listeners(),
            "9.9.9",
            "unknown",
            now(),
            &TimeZone::UTC,
            false,
        );
        let lines: Vec<&str> = screen.lines().collect();
        for line in &lines {
            assert!(line.chars().count() <= WIDTH, "{line}: {screen}");
        }
        let pins = block(&lines, "pins (one per session, the main agent's)");
        assert_eq!(pins.len(), 3, "{screen}");
        // The pin on the long label: the account cell is cut, the state reads through its age.
        let on_long = pins[2];
        assert!(on_long.starts_with("  7b21aa90f3d1  a1 "), "{screen}");
        assert!(on_long.ends_with("  shed from Direct, 12s ago"), "{screen}");
        let account = on_long
            .trim_end_matches("  shed from Direct, 12s ago")
            .rsplit("  ")
            .next()
            .unwrap();
        assert!(
            account.ends_with('…') && LONG_LABEL.starts_with(account.trim_end_matches('…')),
            "{screen}"
        );
        // The pin failed over from the long label: the source is cut, the age stays whole after it.
        let from_long = pins[1];
        assert!(from_long.starts_with("  3f9a0c12e4b7  -  "), "{screen}");
        let source = from_long
            .split("failed over from ")
            .nth(1)
            .and_then(|state| state.strip_suffix(", 12s ago"))
            .unwrap_or_else(|| panic!("the age is cut: {screen}"));
        assert!(
            source.ends_with('…') && LONG_LABEL.starts_with(source.trim_end_matches('…')),
            "{screen}"
        );
    }

    #[test]
    fn a_pin_reads_shed_from_after_a_round_and_failed_over_from_after_a_failover_with_and_without_its_age(
    ) {
        let pin = |session: &str, reason: Value, last_used_at: Value| {
            json!({
                "session_id": session, "agent_id": "main", "account_id": "business",
                "shed": {"from": "direct", "at": NOW_MS - 12_000, "reason": reason}, "pinned": true,
                "model": "claude-sonnet-5", "last_used_at": last_used_at,
                "keepalive": {"state": null, "next_probe_at": null},
            })
        };
        let aged = json!(NOW_MS - 12_000);
        let reads = Reads {
            sessions: Ok(json!({"sessions": [
                pin("1aaaaaaaaaaa0000", json!("round"), aged.clone()),
                pin("2bbbbbbbbbbb0000", json!("cooldown"), aged.clone()),
                pin("3ccccccccccc0000", Value::Null, aged),
                pin("4ddddddddddd0000", json!("round"), Value::Null),
                pin("5eeeeeeeeeee0000", json!("fast_lane"), Value::Null),
            ]})),
            ..labelled("Business", "Direct")
        };
        let screen = render(
            &reads,
            &listeners(),
            "9.9.9",
            "unknown",
            now(),
            &TimeZone::UTC,
            true,
        );
        let lines: Vec<&str> = screen.lines().collect();
        let state = |session: &str| {
            block(&lines, "pins (one per session, the main agent's)")
                .into_iter()
                .find(|line| line.starts_with(&format!("  {session}")))
                .map(|line| line.rsplit("  ").next().unwrap_or_default().to_owned())
                .unwrap_or_else(|| panic!("no line for {session}: {screen}"))
        };
        assert_eq!(
            state("1aaaaaaaaaaa"),
            "shed from Direct, 12s ago",
            "{screen}"
        );
        assert_eq!(
            state("2bbbbbbbbbbb"),
            "failed over from Direct, 12s ago",
            "{screen}"
        );
        assert_eq!(
            state("3ccccccccccc"),
            "shed from Direct, 12s ago",
            "a stamp from before its reason reads as shed: {screen}"
        );
        assert_eq!(state("4ddddddddddd"), "shed from Direct", "{screen}");
        assert_eq!(state("5eeeeeeeeeee"), "failed over from Direct", "{screen}");
    }

    #[test]
    fn a_member_cooldown_prints_its_scope_word_whole_however_long_the_label() {
        // The API-key account alone in its pool: no sample, so its windows read `-` and the cooldown has the most room
        // a member line gives it.
        let mut accounts = fixture("accounts");
        for account in accounts["accounts"].as_array_mut().unwrap() {
            if account["id"] == "direct" {
                account["label"] = json!(LONG_LABEL);
                account["cooldowns"] = json!([{
                    "scope": "model:claude-opus-5", "until": NOW_MS + 11_600_000, "reason": "rate_limited",
                    "window": null, "set_at": NOW_MS,
                }]);
            }
        }
        let reads = Reads {
            accounts: Ok(accounts),
            pools: Ok(json!({"pools": [{
                "id": "lab", "status": "active", "keys": [], "active_pins": 0, "requests_24h": 0,
                "members": [{"account_id": "direct", "priority": 0, "eligible": "cooldown"}],
            }]})),
            ..fixture_reads()
        };
        let screen = render(
            &reads,
            &listeners(),
            "9.9.9",
            "unknown",
            now(),
            &TimeZone::UTC,
            false,
        );
        let lines: Vec<&str> = screen.lines().collect();
        let member = block(&lines, "pools")[1];
        assert!(member.chars().count() <= WIDTH, "{screen}");
        assert!(
            member.contains(&format!(
                "  until {} model:claude-opus-5 ",
                hh_mm(NOW_MS + 11_600_000)
            )),
            "{screen}"
        );
        let label = member.trim_start().split("  ").next().unwrap();
        assert!(
            label.ends_with('…') && LONG_LABEL.starts_with(label.trim_end_matches('…')),
            "{screen}"
        );
    }

    #[test]
    fn the_accounts_in_no_pool_block_lists_an_account_whose_pool_is_null() {
        let mut accounts = fixture("accounts");
        for account in accounts["accounts"].as_array_mut().unwrap() {
            if account["id"] == "direct" {
                account["pool"] = Value::Null;
            }
        }
        let business = accounts["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|account| account["id"] == "business")
            .unwrap();
        assert!(business["pool"].is_object(), "{business}");
        let reads = Reads {
            accounts: Ok(accounts),
            ..fixture_reads()
        };
        let screen = render(
            &reads,
            &listeners(),
            "9.9.9",
            "unknown",
            now(),
            &TimeZone::UTC,
            false,
        );
        let lines: Vec<&str> = screen.lines().collect();
        // The fixture's three reset accounts sit in no pool and every other account in one, so the block lists those
        // three beside the one this case took out of its pool.
        assert_eq!(
            block(&lines, "accounts in no pool"),
            [
                "  Direct          ready",
                "  Reset cooldown  ready",
                "  Reset ending    ready",
                "  Reset used      ready",
            ],
            "{screen}"
        );
    }

    #[test]
    fn a_skipped_probe_is_not_a_failed_one() {
        let ready = |health: Option<Result<SocketAddr, String>>, ready: bool| {
            let mut system = fixture("system");
            system["ready"] = json!(ready);
            Reads {
                health,
                system: Ok(system),
                ..fixture_reads()
            }
        };
        let skipped = ready(None, true);
        assert_eq!(exit_code(&skipped), 0);
        assert_eq!(
            exit_code(&ready(
                Some(Err(
                    "proxy does not answer on 127.0.0.1:8317: refused".to_owned()
                )),
                true
            )),
            1
        );
        assert_eq!(exit_code(&ready(None, false)), 1);
        let unread = Reads {
            system: Err("the admin secret was refused".to_owned()),
            ..fixture_reads()
        };
        assert_eq!(exit_code(&unread), 1, "a system answer that was not read");
    }
}
