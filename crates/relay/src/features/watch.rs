//! The admission of an artifact's watch: the tokens that keyed page reads handed out, by the key that read each one,
//! and the read of a page's answer that finds them.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Write};
use std::sync::{Arc, Mutex, MutexGuard};

use axum::http::HeaderMap;
use phoebis_transport::decode::{decoder_into, encoding, SinkDecoder};
use phoebis_transport::response::ResponseHead;
use sha2::{Digest, Sha256};

use super::plain_path;
use super::socket::Admitted;

/// The code a socket's `401` carries in its DEBUG line and its published end. No `Failure` holds it.
pub(crate) const SOCKET_TOKEN_UNKNOWN: &str = "socket_token_unknown";

/// The entries the map holds for each key. A read past it drops the key's oldest.
const KEY_BOUND: usize = 1_024;

/// The longest token the client accepts.
const TOKEN_MAX: usize = 1_024;

/// The decoded bytes the scan keeps across its writes: the member's name, its colon and quotes, a token of the
/// longest size and a little room for the whitespace JSON allows between them.
const WINDOW: usize = 1_100;

/// An entry's shortest and longest life behind its read, in milliseconds, as the client holds a lease.
const LIFE_MIN_MS: i64 = 300_000;
const LIFE_MAX_MS: i64 = 900_000;

const MEMBER: &[u8] = b"\"syncToken\"";

/// The SHA-256 of a token, under which the map stores it, so the router's memory holds no token.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TokenHash([u8; 32]);

impl TokenHash {
    fn of(token: &str) -> TokenHash {
        TokenHash(Sha256::digest(token.as_bytes()).into())
    }
}

/// One lookup's answer: the identity, and the hash with the stamp its entry was stored under.
pub(crate) struct Entry {
    pub(crate) admitted: Admitted,
    hash: TokenHash,
    stamp: u64,
}

/// Who read a stored token. Private to the map.
enum Readers {
    /// One key: the identity of its latest read, and the key's label as that read took it.
    One { admitted: Admitted, key: String },
    /// Two keys. The entry admits nothing until it ends, whatever reads follow.
    Two,
}

struct Stored {
    ends_at: i64,
    /// The key in whose order the entry stands, the one whose read stored it first.
    owner: KeyId,
    readers: Readers,
    stamp: u64,
}

/// A key as `authenticate_id` reads it: its id and its generation, so a key created again under an id is another.
type KeyId = (String, i64);

fn key_of(admitted: &Admitted) -> KeyId {
    (admitted.key_id.clone(), admitted.key_generation)
}

#[derive(Default)]
struct Tokens {
    entries: HashMap<TokenHash, Stored>,
    /// Each key's hashes in the order they came, the oldest first.
    orders: HashMap<KeyId, VecDeque<TokenHash>>,
    stamps: u64,
}

impl Tokens {
    fn stamp(&mut self) -> u64 {
        self.stamps += 1;
        self.stamps
    }

    fn remove(&mut self, hash: TokenHash) {
        let Some(stored) = self.entries.remove(&hash) else {
            return;
        };
        if let Some(order) = self.orders.get_mut(&stored.owner) {
            order.retain(|held| *held != hash);
            if order.is_empty() {
                self.orders.remove(&stored.owner);
            }
        }
    }

    /// Drops the ended entries in `key`'s order.
    fn sweep(&mut self, key: &KeyId, now: i64) {
        let Tokens {
            entries, orders, ..
        } = self;
        let Some(order) = orders.get_mut(key) else {
            return;
        };
        order.retain(|hash| {
            let live = entries.get(hash).is_some_and(|stored| stored.ends_at > now);
            if !live {
                entries.remove(hash);
            }
            live
        });
        if order.is_empty() {
            orders.remove(key);
        }
    }
}

/// The tokens the keyed page reads handed out, by the SHA-256 of each, in memory alone.
#[derive(Default)]
pub(crate) struct TokenMap(Mutex<Tokens>);

impl TokenMap {
    fn tokens(&self) -> MutexGuard<'_, Tokens> {
        self.0.lock().expect("token map poisoned")
    }

    /// Stores the read's entry under a stamp no earlier entry of the map carries. A token that stands under another
    /// key turns to `Readers::Two` in place, under a new stamp, with one DEBUG line. `key` is the label of the read's
    /// key and `path` the read's path without its query, both for that line.
    pub(crate) fn store(&self, token: &str, admitted: Admitted, key: &str, path: &str, now: i64) {
        if !token_shaped(token) {
            return;
        }
        let hash = TokenHash::of(token);
        let reader = key_of(&admitted);
        let mut guard = self.tokens();
        let tokens = &mut *guard;
        tokens.sweep(&reader, now);
        if tokens
            .entries
            .get(&hash)
            .is_some_and(|stored| stored.ends_at <= now)
        {
            tokens.remove(hash);
        }
        let stamp = tokens.stamp();
        if let Some(stored) = tokens.entries.get_mut(&hash) {
            let Readers::One {
                admitted: held,
                key: first,
            } = &stored.readers
            else {
                return;
            };
            if key_of(held) == reader {
                stored.readers = Readers::One {
                    admitted,
                    key: key.to_owned(),
                };
                stored.ends_at = end_of(token, now);
                stored.stamp = stamp;
                if let Some(order) = tokens.orders.get_mut(&reader) {
                    order.retain(|held| *held != hash);
                    order.push_back(hash);
                }
            } else {
                tracing::debug!(
                    path,
                    first_key = %first,
                    second_key = %key,
                    "a token that two keys read admits no socket"
                );
                // The end and the place in the first key's order stay, so the entry leaves as that key's would.
                stored.readers = Readers::Two;
                stored.stamp = stamp;
            }
            return;
        }
        tokens.entries.insert(
            hash,
            Stored {
                ends_at: end_of(token, now),
                owner: reader.clone(),
                readers: Readers::One {
                    admitted,
                    key: key.to_owned(),
                },
                stamp,
            },
        );
        let order = tokens.orders.entry(reader).or_default();
        order.push_back(hash);
        let oldest = (order.len() > KEY_BOUND)
            .then(|| order.pop_front())
            .flatten();
        if let Some(oldest) = oldest {
            tokens.entries.remove(&oldest);
        }
    }

    /// The entry of the request's token: `None` where the list holds no entry of the token's shape or more than one,
    /// where no entry stands for it, where it has ended, or where two keys read it.
    pub(crate) fn entry(&self, headers: &HeaderMap, now: i64) -> Option<Entry> {
        let hash = TokenHash::of(offered(headers)?);
        let mut tokens = self.tokens();
        let stored = tokens.entries.get(&hash)?;
        if stored.ends_at <= now {
            tokens.remove(hash);
            return None;
        }
        match &stored.readers {
            Readers::One { admitted, .. } => Some(Entry {
                admitted: admitted.clone(),
                hash,
                stamp: stored.stamp,
            }),
            Readers::Two => None,
        }
    }

    /// Removes the entry this lookup read. An entry stored or turned since stands.
    pub(crate) fn forget(&self, entry: &Entry) {
        let mut tokens = self.tokens();
        if tokens
            .entries
            .get(&entry.hash)
            .is_some_and(|stored| stored.stamp == entry.stamp)
        {
            tokens.remove(entry.hash);
        }
    }
}

/// The request's one token: the one entry of the token's shape in its `Sec-WebSocket-Protocol` lines, read as one comma
/// list. The router forwards the caller's lines as sent, so a list that holds a second token, or a line it cannot
/// read, admits nothing it did not check.
fn offered(headers: &HeaderMap) -> Option<&str> {
    let mut found = None;
    for line in headers.get_all("sec-websocket-protocol") {
        for item in line.to_str().ok()?.split(',').map(str::trim) {
            if token_shaped(item) {
                if found.is_some() {
                    return None;
                }
                found = Some(item);
            }
        }
    }
    found
}

/// The client's shape of a token: six dot-joined parts over `[A-Za-z0-9_-]`, at most 1,024 characters.
fn token_shaped(value: &str) -> bool {
    let parts: Vec<&str> = value.split('.').collect();
    (1..=TOKEN_MAX).contains(&value.len())
        && parts.len() == 6
        && parts.iter().all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
}

/// An entry's end, on the relay's clock: the token's part 5 as Unix seconds, held between 300 s and 900 s behind the
/// read, and 900 s where part 5 is no number.
fn end_of(token: &str, now: i64) -> i64 {
    token
        .split('.')
        .nth(4)
        .and_then(|part| part.parse::<i64>().ok())
        .map_or(now + LIFE_MAX_MS, |seconds| {
            seconds
                .saturating_mul(1_000)
                .clamp(now + LIFE_MIN_MS, now + LIFE_MAX_MS)
        })
}

/// The decoded bytes of a page read's answer, searched for the first member `syncToken` with a string value. It
/// keeps the last `WINDOW` bytes across its writes, so a member that straddles two of them is found.
#[derive(Default)]
struct Scan {
    window: Vec<u8>,
    /// The value of the member, a candidate until the body ended clean.
    found: Option<String>,
}

/// What the window holds of a member at one place.
enum Member {
    /// The value, or as much of it as can still be a token where it runs on past one.
    Value(String),
    /// The window ends ahead of the value's end.
    Pending,
    /// Another member, or the name as a value.
    Not,
}

impl Write for Scan {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.found.is_some() {
            return Ok(data.len());
        }
        self.window.extend_from_slice(data);
        let mut from = 0;
        while let Some(at) = self.window[from..]
            .windows(MEMBER.len())
            .position(|bytes| bytes == MEMBER)
            .map(|at| from + at)
        {
            match member_at(&self.window[at + MEMBER.len()..]) {
                Member::Value(value) => {
                    self.found = Some(value);
                    self.window = Vec::new();
                    return Ok(data.len());
                }
                Member::Pending => break,
                Member::Not => from = at + 1,
            }
        }
        let behind = self.window.len().saturating_sub(WINDOW);
        self.window.drain(..behind);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The member whose name ends right ahead of `rest`: whitespace, a colon, whitespace and a string's opening quote,
/// then the value up to its closing quote.
fn member_at(rest: &[u8]) -> Member {
    let mut bytes = rest.iter().copied().enumerate();
    let mut next = || bytes.find(|(_, byte)| !matches!(byte, b' ' | b'\t' | b'\n' | b'\r'));
    match next() {
        Some((_, b':')) => {}
        Some(_) => return Member::Not,
        None => return Member::Pending,
    }
    let start = match next() {
        Some((at, b'"')) => at + 1,
        Some(_) => return Member::Not,
        None => return Member::Pending,
    };
    let value = &rest[start..];
    match value.iter().position(|byte| *byte == b'"') {
        Some(end) => Member::Value(String::from_utf8_lossy(&value[..end]).into_owned()),
        // No token runs this long, so the value is no token whatever follows.
        None if value.len() > TOKEN_MAX => {
            Member::Value(String::from_utf8_lossy(&value[..=TOKEN_MAX]).into_owned())
        }
        None => Member::Pending,
    }
}

/// The tee of one page read's answer, `None` for a call that hands out no token.
pub(crate) struct Tee {
    admitted: Admitted,
    /// `None` once a decode failed, which ends the tee with nothing stored.
    decoder: Option<SinkDecoder<Scan>>,
    key: String,
    map: Arc<TokenMap>,
    now: i64,
    path: String,
}

impl Tee {
    /// The tee of a keyed `GET` of `/api/frame/` and one segment other than `sync` that the upstream answered `200`,
    /// whatever its query. `key` is the label of the read's key and `path` the read's path without its query.
    pub(crate) fn of(
        map: &Arc<TokenMap>,
        method: &str,
        path: &str,
        head: &ResponseHead,
        admitted: &Admitted,
        key: &str,
        now: i64,
    ) -> Option<Tee> {
        let page = plain_path(path).is_some_and(|plain| {
            plain
                .strip_prefix(b"/api/frame/")
                .is_some_and(|slug| !slug.is_empty() && !slug.contains(&b'/') && slug != b"sync")
        });
        if method != "GET" || !page || head.status != 200 {
            return None;
        }
        let Ok(coding) = encoding(head) else {
            unscanned("unknown_coding", path);
            return None;
        };
        Some(Tee {
            admitted: admitted.clone(),
            decoder: Some(decoder_into(coding, Scan::default())),
            key: key.to_owned(),
            map: Arc::clone(map),
            now,
            path: path.to_owned(),
        })
    }

    /// One piece of the body. Stores nothing.
    pub(crate) fn feed(&mut self, piece: &[u8]) {
        let Some(decoder) = &mut self.decoder else {
            return;
        };
        if decoder.sink().found.is_some() {
            return;
        }
        if decoder.feed(piece).is_err() {
            unscanned("undecodable", &self.path);
            self.decoder = None;
        }
    }

    /// The body's clean end: scans what the decoder kept back and stores a found token. A tee that is dropped without
    /// this call stores nothing.
    pub(crate) fn end(self) {
        let Some(mut decoder) = self.decoder else {
            return;
        };
        let found = match decoder.sink().found.take() {
            Some(found) => found,
            None => match decoder.finish() {
                Ok(scan) => match scan.found {
                    Some(found) => found,
                    None => return,
                },
                Err(_) => {
                    unscanned("undecodable", &self.path);
                    return;
                }
            },
        };
        self.map
            .store(&found, self.admitted, &self.key, &self.path, self.now);
    }
}

/// The tee's one line where it ends with nothing scanned: the cause and the path, and no byte of the body.
fn unscanned(cause: &'static str, path: &str) {
    tracing::debug!(cause, path, "a page read's answer was not scanned");
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoebis_store::Feature;

    fn admitted() -> Admitted {
        Admitted {
            account_id: "a".into(),
            feature: Feature::Artifacts,
            key_generation: 1,
            key_id: "desktop".into(),
            sign_in: (1, 1),
        }
    }

    #[test]
    fn a_removal_spares_an_entry_stored_since() {
        let token = "aaa.bbb.owner_agent.1000.1600.sig";
        let mut headers = HeaderMap::new();
        headers.insert(
            "sec-websocket-protocol",
            format!("frame-sync.v1,{token}").parse().unwrap(),
        );
        let map = TokenMap::default();
        map.store(token, admitted(), "desktop", "/api/frame/x", 1_000_000);
        let first = map.entry(&headers, 1_000_000).unwrap();
        map.forget(&first);
        map.store(token, admitted(), "desktop", "/api/frame/x", 1_000_000);
        map.forget(&first);
        assert!(map.entry(&headers, 1_000_000).is_some());
    }
}
