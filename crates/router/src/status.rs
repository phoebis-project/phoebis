//! `phoebis status`: the proxy listener's readiness probe, then five admin reads through the subcommand client,
//! printed as one screen of at most 80 columns or as one JSON object.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use hyper::Method;
use jiff::tz::TimeZone;
use jiff::Timestamp;
use serde_json::{json, Value};

use crate::admin_client::{AdminClient, ClientError, Common};
use crate::health;

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

/// The three listeners as the file names them, with their schemes, and the CA path under `self_signed`.
pub struct Listeners {
    pub admin: String,
    pub ca: Option<PathBuf>,
    pub metrics: Option<String>,
    pub proxy: Option<String>,
}

/// The lines the `pins` and `accounts in no pool` blocks print before `and N more`, unless `--all`.
pub const BLOCK_CAP: usize = 20;

/// The screen's width in columns; no line passes it.
pub const WIDTH: usize = 80;

/// The widest a label, an account id or a pool id prints in its column before it ends in `…`.
const LABEL_CAP: usize = 20;

/// The widest an agent id prints in the pins block.
const AGENT_CAP: usize = 10;

/// How many characters of a session id the pins block prints.
const SESSION_CHARS: usize = 12;

/// Where a wrapped line carries on.
const CARRY: &str = "    ";

/// The probe (blocking, on a worker) and the five GETs, in the spec's order, each under `deadline`.
pub async fn read(
    client: &AdminClient,
    proxy: Option<(SocketAddr, bool)>,
    deadline: Duration,
) -> Reads {
    let health = match proxy {
        // The probe keeps its own three-second budget, so `health` and `status` read the listener the same way, and
        // a shorter `--timeout` shortens it too.
        Some((listen, tls)) => {
            let budget = health::TIMEOUT.min(deadline);
            Some(
                tokio::task::spawn_blocking(move || health::probe(listen, tls, budget))
                    .await
                    .unwrap_or_else(|error| {
                        Err(format!("the proxy probe did not finish: {error}"))
                    }),
            )
        }
        None => None,
    };
    let mut reader = Reader {
        client,
        deadline,
        refused: None,
    };
    let system = reader.get("/admin/system").await;
    let attention = reader.get("/admin/attention").await;
    let pools = reader.get("/admin/pools").await;
    let accounts = reader.get("/admin/accounts").await;
    let sessions = reader.get("/admin/sessions").await;
    Reads {
        accounts,
        attention,
        health,
        pools,
        sessions,
        system,
    }
}

/// The five GETs one after another, each under `deadline`.
struct Reader<'a> {
    client: &'a AdminClient,
    deadline: Duration,
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
        let deadline = self.deadline;
        match tokio::time::timeout(deadline, self.client.expect(Method::GET, path, None)).await {
            Ok(Ok(answer)) => Ok(answer.body),
            Ok(Err(ClientError::Transport { url, reason })) => Err(format!(
                "the admin listener did not answer on {url}: {reason}"
            )),
            Ok(Err(ClientError::Refused { status, message })) => {
                let line = format!("the admin listener answered {status} {message}");
                if status == 429 {
                    self.refused = Some(line.clone());
                }
                Err(line)
            }
            Ok(Err(error @ ClientError::Refused401)) => {
                let line = error.to_string();
                self.refused = Some(line.clone());
                Err(line)
            }
            Ok(Err(error)) => Err(error.to_string()),
            Err(_) => {
                let base = self.client.base();
                Err(format!(
                    "the admin listener did not answer on {}://{}: the call did not finish within {deadline:?}",
                    base.scheme_str().unwrap_or_default(),
                    base.authority().map(|a| a.as_str()).unwrap_or_default()
                ))
            }
        }
    }
}

/// The screen. `now` and `zone` are the clock and the local zone, passed in so a test renders a fixed instant.
pub fn render(
    reads: &Reads,
    listeners: &Listeners,
    binary_version: &str,
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
    if reads.health.is_none() {
        screen.push("  proxy: not probed, no configuration file names the listener");
    }
    if let Some(ca) = listeners.ca.as_ref().filter(|ca| ca.is_file()) {
        screen.push(format!("  tls: clients trust {}", ca.display()));
    }
    match &reads.system {
        Ok(system) => {
            if let Some(line) = ledger_line(system) {
                screen.push(format!("  {line}"));
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
                        clock.time(&item["since"])
                    )
                })
                .collect();
            if items.is_empty() {
                screen.push("  attention: none");
            } else {
                screen.flow("  attention: ", &items, "; ", CARRY);
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
            let mut unpooled: Vec<(String, &str)> = answer["accounts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|account| account["pools"].as_array().is_none_or(Vec::is_empty))
                .map(|account| (accounts.label(text(&account["id"])), state_word(account)))
                .collect();
            if !unpooled.is_empty() {
                unpooled.sort();
                screen.blank();
                screen.push("accounts in no pool");
                let rows: Vec<Vec<String>> = unpooled
                    .into_iter()
                    .map(|(label, state)| vec![cut(&label, LABEL_CAP), state.to_owned()])
                    .collect();
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

/// `ledger: ...` naming each of the ledger's and the state writer's figures that is non-zero or true, or `None` when
/// all of them read clean.
fn ledger_line(system: &Value) -> Option<String> {
    let ledger = &system["ledger"];
    let writer = &system["router"]["writer"];
    let count = |value: &Value, word: &str| {
        value
            .as_u64()
            .filter(|n| *n > 0)
            .map(|n| format!("{n} {word}"))
    };
    let flag = |value: &Value, word: &str| (*value == true).then(|| word.to_owned());
    let ledger_items: Vec<String> = [
        count(&ledger["queued"], "queued"),
        flag(&ledger["behind"], "behind"),
        flag(&ledger["degraded"], "degraded"),
        count(&ledger["parked_priced"], "parked"),
        count(&ledger["retrying"], "retrying"),
    ]
    .into_iter()
    .flatten()
    .collect();
    let writer_items: Vec<String> = [
        count(&writer["deferred"], "deferred"),
        flag(&writer["degraded"], "degraded"),
    ]
    .into_iter()
    .flatten()
    .collect();
    let mut groups = Vec::new();
    if !ledger_items.is_empty() {
        groups.push(ledger_items.join(", "));
    }
    if !writer_items.is_empty() {
        groups.push(format!("writer: {}", writer_items.join(", ")));
    }
    (!groups.is_empty()).then(|| format!("ledger: {}", groups.join("; ")))
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
    let headers: Vec<Vec<String>> = pools
        .iter()
        .map(|pool| {
            let members = pool["members"].as_array().map_or(0, Vec::len) as u64;
            let keys = pool["keys"].as_array().map_or(0, Vec::len) as u64;
            let pins = pool["active_pins"].as_u64().unwrap_or(0);
            let requests = pool["requests_24h"].as_u64().unwrap_or(0);
            vec![
                cut(text(&pool["id"]), LABEL_CAP),
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
    for (pool, header) in pools.iter().zip(table("  ", headers)) {
        screen.push(header);
        let mut members: Vec<&Value> = pool["members"].as_array().into_iter().flatten().collect();
        members.sort_by(|a, b| {
            a["priority"]
                .as_i64()
                .cmp(&b["priority"].as_i64())
                .then_with(|| text(&a["account_id"]).cmp(text(&b["account_id"])))
        });
        let rows: Vec<Vec<String>> = members
            .iter()
            .map(|member| {
                let id = text(&member["account_id"]);
                let account = accounts.get(id);
                let mut row = vec![
                    cut(&accounts.label(id), LABEL_CAP),
                    account.map_or("-", state_word).to_owned(),
                    text(&member["eligible"]).to_owned(),
                ];
                if let Some(account) = account {
                    row.extend(windows_cells(account, clock));
                }
                row
            })
            .collect();
        for line in table("    ", rows) {
            screen.push(line);
        }
    }
}

/// `5h NN%` and `7d NN%` (`-` with no sample), `fable NN%` when the account has a Fable week, then `until HH:MM
/// <reason>` under a cooldown or `resets HH:MM` for the nearest reset to come.
fn windows_cells(account: &Value, clock: &Clock) -> Vec<String> {
    let windows = windows_of(account);
    let percent = |id: &str| {
        windows
            .get(id)
            .and_then(|w| w["utilization"].as_f64())
            .map_or("-".to_owned(), |u| {
                format!("{}%", (u * 100.0).round() as i64)
            })
    };
    let mut cells = vec![
        format!("5h {}", percent("5h")),
        format!("7d {}", percent("7d")),
    ];
    if windows.contains_key("fable") {
        cells.push(format!("fable {}", percent("fable")));
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
        cells.push(format!(
            "until {} {}",
            clock.time(&cooldown["until"]),
            text(&cooldown["reason"])
        ));
    } else if let Some(reset) = windows
        .values()
        .filter_map(|w| w["resets_at"].as_i64())
        .filter(|at| *at >= now)
        .min()
    {
        cells.push(format!("resets {}", clock.time(&json!(reset))));
    }
    cells
}

/// The account's windows under the three ids the screen prints, mapped as the console maps the wire's spellings,
/// the newest sample winning when two spellings name one window.
fn windows_of(account: &Value) -> BTreeMap<&'static str, &Value> {
    let mut windows: BTreeMap<&'static str, &Value> = BTreeMap::new();
    for window in account["windows"].as_array().into_iter().flatten() {
        let id = match text(&window["id"]) {
            "5h" | "five_hour" => "5h",
            "7d" | "seven_day" => "7d",
            "7d_oi" | "fable" | "seven_day_fable" | "seven_day_overage_included" => "fable",
            _ => continue,
        };
        let newer = windows
            .get(id)
            .is_none_or(|seen| window["observed_at"].as_i64() > seen["observed_at"].as_i64());
        if newer {
            windows.insert(id, window);
        }
    }
    windows
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
    rows.extend(pins.iter().map(|row| {
        let age = row["last_used_at"].as_i64().map(|at| clock.age(at));
        let mut state = match (row["shed_from"].as_str(), age) {
            (Some(from), Some(age)) => format!("shedding from {}, {age} ago", accounts.label(from)),
            (Some(from), None) => format!("shedding from {}", accounts.label(from)),
            (None, Some(age)) => format!("pinned {age} ago, {}", text(&row["model"])),
            (None, None) => format!("pinned, {}", text(&row["model"])),
        };
        let keepalive = &row["keepalive"];
        if let Some(keepalive_state) = keepalive["state"].as_str() {
            state.push_str(&format!(", keepalive {keepalive_state}"));
            if !keepalive["next_probe_at"].is_null() {
                state.push_str(&format!(
                    " next probe {}",
                    clock.time(&keepalive["next_probe_at"])
                ));
            }
        }
        vec![
            text(&row["session_id"])
                .chars()
                .take(SESSION_CHARS)
                .collect(),
            cut(named_or(&row["agent_id"], &Value::Null), AGENT_CAP),
            cut(&accounts.label(text(&row["account_id"])), LABEL_CAP),
            state,
        ]
    }));
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

/// `paused` for an account routing is turned off on, else the account's `state`.
fn state_word(account: &Value) -> &str {
    if account["enabled"] == false {
        "paused"
    } else {
        text(&account["state"])
    }
}

/// The clock the screen's times and ages read from, in the zone they print in.
struct Clock<'a> {
    now: Timestamp,
    zone: &'a TimeZone,
}

impl Clock<'_> {
    /// An epoch-millisecond instant as `HH:MM` in the zone, `MM-DD HH:MM` when it falls on another day than now.
    fn time(&self, at: &Value) -> String {
        let Some(at) = at
            .as_i64()
            .and_then(|ms| Timestamp::from_millisecond(ms).ok())
        else {
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

    fn listeners(ca: Option<PathBuf>) -> Listeners {
        Listeners {
            admin: "http://127.0.0.1:8081".to_owned(),
            ca,
            metrics: Some("http://127.0.0.1:9464".to_owned()),
            proxy: Some("https://0.0.0.0:8317".to_owned()),
        }
    }

    /// The fixtures' own clock: their `since`, `set_at` and `last_used_at` read 1,000,000 ms, 00:16 UTC.
    fn now() -> Timestamp {
        Timestamp::from_millisecond(1_000_000).unwrap()
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

    /// The fixture's pinned row as `count` pinned sessions, every fifth shedding from `direct` and every third with a
    /// keepalive, beside the fixture's unpinned rows.
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
            row["last_used_at"] = json!(1_000_000 - i as i64 * 60_000);
            if i % 5 == 0 {
                row["shed_from"] = json!("direct");
            }
            if i % 3 == 0 {
                row["keepalive"]["state"] = json!("idle");
                row["keepalive"]["next_probe_at"] = json!(1_200_000);
            }
            rows.push(row);
        }
        json!({ "sessions": rows })
    }

    #[test]
    fn the_screen_renders_the_captured_answers_in_eighty_columns() {
        let dir = tempfile::tempdir().unwrap();
        let ca = dir.path().join("ca.pem");
        std::fs::write(&ca, "").unwrap();
        let reads = fixture_reads();
        let screen = render(
            &reads,
            &listeners(Some(ca.clone())),
            "9.9.9",
            now(),
            &TimeZone::UTC,
            false,
        );
        let lines: Vec<&str> = screen.lines().collect();
        for line in &lines {
            assert!(line.chars().count() <= WIDTH, "{line}");
        }

        let system = fixture("system");
        let dead: Vec<&str> = system["dead"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d.as_str().unwrap())
            .collect();
        assert!(
            lines[0].starts_with(&format!(
                "phoebis {}, up ",
                system["version"].as_str().unwrap()
            )),
            "{screen}"
        );
        if dead.is_empty() {
            assert!(lines[0].ends_with(", ready"), "{screen}");
        } else {
            assert!(
                lines[0].ends_with(&format!("not ready: {}", dead.join(", "))),
                "{screen}"
            );
        }
        assert!(
            lines.contains(&format!("  tls: clients trust {}", ca.display()).as_str()),
            "{screen}"
        );
        assert!(lines.contains(&"  ledger: 1 parked"), "{screen}");

        let attention: String = lines
            .iter()
            .skip_while(|line| !line.starts_with("  attention: "))
            .take_while(|line| !line.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" ");
        let items = fixture("attention")["items"].as_array().unwrap().clone();
        assert!(!items.is_empty());
        for item in &items {
            let named = format!(
                "{} {} since 00:16",
                item["kind"].as_str().unwrap(),
                item["subject"]["label"].as_str().unwrap()
            );
            assert_eq!(attention.matches(&named).count(), 1, "{named}: {screen}");
        }

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
        let mut pools = fixture("pools")["pools"].as_array().unwrap().clone();
        pools.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        let mut at = 0;
        for pool in &pools {
            let header = pools_block[at];
            assert!(
                header.starts_with(&format!("  {} ", pool["id"].as_str().unwrap()))
                    && header.contains(pool["status"].as_str().unwrap()),
                "{screen}"
            );
            let mut members = pool["members"].as_array().unwrap().clone();
            members.sort_by_key(|m| m["priority"].as_i64());
            for (offset, member) in members.iter().enumerate() {
                let line = pools_block[at + 1 + offset];
                assert!(
                    line.starts_with("    ")
                        && line.contains(&label_of(member["account_id"].as_str().unwrap())),
                    "{line}: {screen}"
                );
            }
            at += 1 + members.len();
        }
        assert_eq!(
            at,
            pools_block.len(),
            "one line per pool and member: {screen}"
        );
        // The windows and the cooldown of the first member, cut at the width, and the dashes of a member with no
        // sample.
        assert!(
            pools_block[1].contains("5h 62%")
                && pools_block[1].contains("7d 41%")
                && pools_block[1].contains("fable 12%")
                && pools_block[1].contains("until 00:46 model_rej")
                && pools_block[1].ends_with('…'),
            "{screen}"
        );
        assert!(
            pools_block[2].contains("5h -") && pools_block[2].contains("7d -"),
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
        for (line, row) in pins[1..].iter().zip(&pinned) {
            assert!(
                line.contains(&label_of(row["account_id"].as_str().unwrap()))
                    && line.contains("pinned"),
                "{line}"
            );
            assert_eq!(
                line.contains("shedding from"),
                !row["shed_from"].is_null(),
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
            &listeners(None),
            "9.9.9",
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
            &listeners(None),
            "9.9.9",
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
        assert_eq!(
            whole_pins
                .iter()
                .filter(|line| line.contains("shedding from Direct, "))
                .count(),
            5,
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
        row["shed_from"] = Value::Null;
        let mut one = fixture_reads();
        one.sessions = Ok(short);
        let one_screen = render(
            &one,
            &listeners(None),
            "9.9.9",
            now(),
            &TimeZone::UTC,
            false,
        );
        let one_lines: Vec<&str> = one_screen.lines().collect();
        assert_eq!(
            block(&one_lines, "pins (one per session, the main agent's)"),
            [
                "  session   agent  account  state",
                "  s-ext-00  -      Direct   pinned 0s ago, m, keepalive idle next probe 00:20"
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
        for name in ["system", "attention", "pools", "accounts", "sessions"] {
            assert_eq!(json[name], fixture(name), "{name}");
        }
        assert_eq!(json["errors"], json!({}));
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

        let file_free = Listeners {
            admin: "http://127.0.0.1:8081".to_owned(),
            ca: None,
            metrics: None,
            proxy: None,
        };
        let screen = render(&skipped, &file_free, "9.9.9", now(), &TimeZone::UTC, false);
        let lines: Vec<&str> = screen.lines().collect();
        assert!(lines[0].ends_with(", ready"), "{screen}");
        assert_eq!(lines[1], "  admin http://127.0.0.1:8081", "{screen}");
        assert_eq!(
            lines[2], "  proxy: not probed, no configuration file names the listener",
            "{screen}"
        );
        assert!(render_json(&skipped)["health"].is_null());
    }
}
