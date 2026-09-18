//! Failed authentications, counted per client address in a sliding minute.
//! An address that fails too often is not looked at until its window
//! clears, right keys included: `UNAUTHENTICATED` must not be a free oracle.
//!
//! Only failures are counted, and only while the address is not limited, so
//! an address holds at most `per_minute` timestamps. The table of addresses
//! is bounded as well; when it is full, the address that failed longest ago
//! makes room.

use std::collections::{BTreeMap, VecDeque};
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::{Mutex, PoisonError};

use axum::http::HeaderMap;

/// The window failures are counted in, in seconds.
pub const WINDOW: u64 = 60;

/// How many addresses are remembered at most.
pub const CAPACITY: usize = 10_000;

/// The limiter. The time is its caller's to tell, in seconds.
#[derive(Debug)]
pub struct FailureLimiter {
    per_minute: usize,
    capacity: usize,
    table: Mutex<BTreeMap<IpAddr, VecDeque<u64>>>,
}

/// An IPv6 client has a /64 of addresses to itself, as a rule, so that is
/// what is counted; an IPv4-mapped address is the IPv4 address it maps.
fn counted_as(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V4(_) => address,
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let mut segments = v6.segments();
                segments[4..].fill(0);
                IpAddr::V6(Ipv6Addr::from(segments))
            }
        },
    }
}

impl FailureLimiter {
    /// `per_minute` of 0 turns the limiter off.
    pub fn new(per_minute: u32, capacity: usize) -> Self {
        Self {
            per_minute: usize::try_from(per_minute).unwrap_or(usize::MAX),
            capacity: capacity.max(1),
            table: Mutex::new(BTreeMap::new()),
        }
    }

    /// `Some(seconds)` while `address` is limited: how long until there is
    /// room for another attempt.
    pub fn limited(&self, address: IpAddr, now: u64) -> Option<u64> {
        if self.per_minute == 0 {
            return None;
        }
        let mut table = self.table.lock().unwrap_or_else(PoisonError::into_inner);
        let address = counted_as(address);
        let failures = table.get_mut(&address)?;
        forget_old(failures, now);
        if failures.is_empty() {
            table.remove(&address);
            return None;
        }
        let oldest = *failures.front()?;
        (failures.len() >= self.per_minute).then(|| (oldest + WINDOW).saturating_sub(now).max(1))
    }

    /// Counts a failed authentication of `address`.
    pub fn failed(&self, address: IpAddr, now: u64) {
        if self.per_minute == 0 {
            return;
        }
        let mut table = self.table.lock().unwrap_or_else(PoisonError::into_inner);
        let address = counted_as(address);
        if !table.contains_key(&address) && table.len() >= self.capacity {
            make_room(&mut table, self.capacity, now);
        }
        let failures = table.entry(address).or_default();
        forget_old(failures, now);
        if failures.len() < self.per_minute {
            failures.push_back(now);
        }
    }

    /// How many addresses are remembered.
    pub fn addresses(&self) -> usize {
        self.table
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

fn forget_old(failures: &mut VecDeque<u64>, now: u64) {
    while failures.front().is_some_and(|at| now >= at + WINDOW) {
        failures.pop_front();
    }
}

/// Drops what has cleared; if nothing has, the address whose last failure
/// is the oldest.
fn make_room(table: &mut BTreeMap<IpAddr, VecDeque<u64>>, capacity: usize, now: u64) {
    table.retain(|_, failures| failures.back().is_some_and(|at| now < at + WINDOW));
    if table.len() < capacity {
        return;
    }
    let stalest = table
        .iter()
        .min_by_key(|(_, failures)| failures.back().copied().unwrap_or(0))
        .map(|(address, _)| *address);
    if let Some(stalest) = stalest {
        table.remove(&stalest);
    }
}

/// The client's address (PLAN-00004 D-03). Behind a proxy it is the last
/// entry of `X-Forwarded-For`: the one the proxy itself appended. Whatever
/// stands before it came from the client and may be forged. Without the
/// header, or with no address in that place, it is the peer: the proxy.
/// When no proxy is configured the header is not read at all.
pub fn client_address(peer: SocketAddr, headers: &HeaderMap, behind_proxy: bool) -> IpAddr {
    if !behind_proxy {
        return peer.ip();
    }
    headers
        .get_all("x-forwarded-for")
        .iter()
        .next_back()
        .and_then(|value| value.to_str().ok())
        .and_then(|chain| chain.rsplit(',').next())
        .and_then(|last| last.trim().parse().ok())
        .unwrap_or_else(|| peer.ip())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn the_limit_is_reached_and_clears_as_the_window_slides() {
        let limiter = FailureLimiter::new(3, 10);
        let a = ip("192.0.2.1");
        for now in [100, 110, 120] {
            assert_eq!(limiter.limited(a, now), None);
            limiter.failed(a, now);
        }
        assert_eq!(limiter.limited(a, 120), Some(40));
        assert_eq!(limiter.limited(a, 159), Some(1));
        // Attempts while limited are not counted: they were not looked at.
        limiter.failed(a, 159);
        assert_eq!(limiter.limited(a, 160), None, "the first has cleared");
        limiter.failed(a, 160);
        assert_eq!(limiter.limited(a, 160), Some(10), "until the second clears");
        assert_eq!(limiter.limited(ip("192.0.2.2"), 160), None);
        // Once everything has cleared, the address is forgotten.
        assert_eq!(limiter.limited(a, 1_000), None);
        assert_eq!(limiter.addresses(), 0);
    }

    #[test]
    fn a_limit_of_zero_is_no_limit() {
        let limiter = FailureLimiter::new(0, 10);
        for _ in 0..100 {
            limiter.failed(ip("192.0.2.1"), 5);
        }
        assert_eq!(limiter.limited(ip("192.0.2.1"), 5), None);
        assert_eq!(limiter.addresses(), 0);
    }

    #[test]
    fn the_table_is_bounded() {
        let limiter = FailureLimiter::new(2, 3);
        for (n, now) in [(1, 10), (2, 20), (3, 30)] {
            limiter.failed(ip(&format!("192.0.2.{n}")), now);
            limiter.failed(ip(&format!("192.0.2.{n}")), now);
        }
        assert_eq!(limiter.addresses(), 3);
        // Full, and nothing has cleared: whoever failed longest ago goes.
        limiter.failed(ip("192.0.2.4"), 40);
        assert_eq!(limiter.addresses(), 3);
        assert_eq!(limiter.limited(ip("192.0.2.1"), 40), None);
        assert!(limiter.limited(ip("192.0.2.2"), 40).is_some());
        // Full, and the others have cleared: they all go.
        limiter.failed(ip("192.0.2.5"), 95);
        assert_eq!(limiter.addresses(), 2);
        for n in 0..1_000_u32 {
            limiter.failed(IpAddr::V4(std::net::Ipv4Addr::from(n)), 100);
        }
        assert_eq!(limiter.addresses(), 3);
    }

    #[test]
    fn an_ipv6_client_is_its_slash_64() {
        let limiter = FailureLimiter::new(2, 10);
        limiter.failed(ip("2001:db8:1:2::1"), 0);
        limiter.failed(ip("2001:db8:1:2:ffff::9"), 0);
        assert!(limiter.limited(ip("2001:db8:1:2:abcd::"), 0).is_some());
        assert_eq!(limiter.limited(ip("2001:db8:1:3::1"), 0), None);
        assert_eq!(limiter.addresses(), 1);
        // An IPv4 client that arrives on a dual-stack socket is itself.
        limiter.failed(ip("::ffff:192.0.2.7"), 0);
        limiter.failed(ip("192.0.2.7"), 0);
        assert!(limiter.limited(ip("192.0.2.7"), 0).is_some());
    }

    #[test]
    fn the_client_address_is_the_peer_or_what_the_proxy_appended() {
        let peer: SocketAddr = "10.0.0.2:4711".parse().unwrap();
        let mut headers = HeaderMap::new();
        assert_eq!(client_address(peer, &headers, true), ip("10.0.0.2"));
        headers.append(
            "x-forwarded-for",
            HeaderValue::from_static("6.6.6.6, 203.0.113.9"),
        );
        assert_eq!(client_address(peer, &headers, true), ip("203.0.113.9"));
        assert_eq!(client_address(peer, &headers, false), ip("10.0.0.2"));
        // A second header line comes after the first.
        headers.append("x-forwarded-for", HeaderValue::from_static("2001:db8::5"));
        assert_eq!(client_address(peer, &headers, true), ip("2001:db8::5"));
        for rubbish in ["", "unknown", "203.0.113.9,", "203.0.113.9:80"] {
            let mut headers = HeaderMap::new();
            headers.append("x-forwarded-for", HeaderValue::from_str(rubbish).unwrap());
            assert_eq!(
                client_address(peer, &headers, true),
                ip("10.0.0.2"),
                "{rubbish:?}"
            );
        }
    }
}
