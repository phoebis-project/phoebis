//! Where a webhook may point. The router posts wherever a rule aims it and follows no redirect, so a
//! destination only this host can reach would turn it into a probe of its own network. The classes are named
//! once here: the admin route reads them off a literal host when a rule is written, and the deliverer reads
//! them off every answer DNS gives a name, on every attempt.

use std::error::Error;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// The refusal words, which the route puts in its refusal and the deliverer in its attempt error.
pub const LINK_LOCAL: &str = "a link-local address";
pub const LOOPBACK: &str = "loopback";
pub const UNSPECIFIED: &str = "the unspecified address";

pub fn ipv4_refusal(ip: Ipv4Addr) -> Option<&'static str> {
    if ip.is_loopback() {
        Some(LOOPBACK)
    } else if ip.is_link_local() {
        Some(LINK_LOCAL)
    } else if ip.is_unspecified() {
        Some(UNSPECIFIED)
    } else {
        None
    }
}

pub fn ipv6_refusal(ip: Ipv6Addr) -> Option<&'static str> {
    // `::1` and `::` first: the fold below reads them as 0.0.0.1 and 0.0.0.0, which loses the loopback class.
    if ip.is_loopback() {
        return Some(LOOPBACK);
    }
    if ip.is_unspecified() {
        return Some(UNSPECIFIED);
    }
    // An IPv4-mapped address, and the deprecated IPv4-compatible form `to_ipv4` also covers, connects straight
    // to the embedded IPv4 host, so classify that host instead.
    if let Some(embedded) = ip.to_ipv4_mapped().or_else(|| ip.to_ipv4()) {
        return ipv4_refusal(embedded);
    }
    // Matching fe80::/10 by hand: `Ipv6Addr::is_unicast_link_local` is still unstable.
    (ip.segments()[0] & 0xffc0 == 0xfe80).then_some(LINK_LOCAL)
}

pub fn address_refusal(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(ip) => ipv4_refusal(ip),
        IpAddr::V6(ip) => ipv6_refusal(ip),
    }
}

/// What a delivery may connect to out of one DNS answer: the refused classes are dropped rather than the
/// whole answer, so a name that answers with a public address beside a local one is still reached at the
/// public one. An answer with nothing left is the refusal itself.
pub fn admitted(
    answer: impl IntoIterator<Item = SocketAddr>,
    allow_local: bool,
) -> Result<Vec<SocketAddr>, &'static str> {
    if allow_local {
        return Ok(answer.into_iter().collect());
    }
    let mut kept = Vec::new();
    let mut refused = None;
    for address in answer {
        match address_refusal(address.ip()) {
            None => kept.push(address),
            Some(reason) => refused = refused.or(Some(reason)),
        }
    }
    match refused {
        Some(reason) if kept.is_empty() => Err(reason),
        _ => Ok(kept),
    }
}

/// The attempt's error when every address a name answered with was refused. Carried down the connector's
/// source chain so the delivery row says why rather than reporting a plain connect failure.
#[derive(Debug)]
pub struct Refused(&'static str);

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the destination resolved to {}", self.0)
    }
}

impl Error for Refused {}

/// The refusal behind a failed request, or `None` when it failed for any other reason.
pub fn refusal_of(error: &dyn Error) -> Option<String> {
    let mut source = error.source();
    while let Some(error) = source {
        if let Some(refused) = error.downcast_ref::<Refused>() {
            return Some(refused.to_string());
        }
        source = error.source();
    }
    None
}

/// Resolution with the destination policy applied to the answer. A rule names a host, and the address behind
/// that host is the operator's to change at any time, so the classes are checked here, on the attempt that
/// connects, rather than once when the rule was written.
pub struct GuardedResolver {
    allow_local: bool,
}

impl GuardedResolver {
    pub fn new(allow_local: bool) -> GuardedResolver {
        GuardedResolver { allow_local }
    }
}

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let allow_local = self.allow_local;
        let host = name.as_str().to_string();
        Box::pin(async move {
            // Port zero: the URL's own port replaces it.
            let answer: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            match admitted(answer, allow_local) {
                Ok(addresses) => Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs),
                Err(reason) => Err(Box::new(Refused(reason)) as Box<dyn Error + Send + Sync>),
            }
        })
    }
}
