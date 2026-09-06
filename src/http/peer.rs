//! Client identity: one normalized peer resolution per request.
//!
//! Lives in `http/` (not `state.rs`): it reads HTTP transport inputs
//! (`ConnectInfo`, proxy headers) while `state.rs` owns the `Limiter` store.
//!
//! Every consumer of "the client's IP" — governors (via
//! [`ClientIdentityExtractor`]), the in-memory `Limiter` (via
//! [`ClientIdentity::limiter_key`]), and IP hashing — goes through this
//! value, so proxy handling flips in one place without touching handlers:
//!
//! - `TRUST_PROXY` unset (default): `X-Forwarded-For`, `X-Real-IP`, and
//!   `Forwarded` are ignored entirely; identity is the normalized TCP peer
//!   (spoof-proof).
//! - `TRUST_PROXY` set: the leftmost valid `X-Forwarded-For` entry is the
//!   client; else `X-Real-IP`; else the first `Forwarded for=`; else the
//!   peer. The proxy in front must overwrite (not merely append to) these
//!   headers, or clients can spoof each other's identity and quota.
//!
//! IPv4-mapped IPv6 addresses (`::ffff:1.2.3.4`) canonicalize to IPv4, so one
//! client never gets two buckets, two quota keys, or two hashes.

use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::HeaderMap;
use axum::http::request::{Parts, Request};
use tower_governor::GovernorError;
use tower_governor::key_extractor::KeyExtractor;

use crate::error::AppError;
use crate::state::AppState;

/// The normalized identity of the caller behind one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientIdentity {
    ip: IpAddr,
}

impl ClientIdentity {
    /// Canonicalize one address: IPv4-mapped IPv6 collapses to IPv4;
    /// everything else passes through untouched.
    pub fn normalize_ip(ip: IpAddr) -> IpAddr {
        match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
            v4 => v4,
        }
    }

    /// Identity straight from the TCP peer (normalizes mapped → v4).
    pub fn from_peer(addr: SocketAddr) -> Self {
        Self {
            ip: Self::normalize_ip(addr.ip()),
        }
    }

    /// Resolve the request's identity. Returns `None` only when there is no
    /// peer address and (with `trust_proxy`) no usable header — practically
    /// unreachable behind `into_make_service_with_connect_info`.
    pub fn resolve(
        peer: Option<SocketAddr>,
        headers: &HeaderMap,
        trust_proxy: bool,
    ) -> Option<Self> {
        if trust_proxy {
            if let Some(ip) = maybe_x_forwarded_for(headers)
                .or_else(|| maybe_x_real_ip(headers))
                .or_else(|| maybe_forwarded(headers))
            {
                return Some(Self {
                    ip: Self::normalize_ip(ip),
                });
            }
        }
        peer.map(Self::from_peer)
    }

    /// The normalized address behind this identity.
    pub fn ip(&self) -> IpAddr {
        self.ip
    }

    /// The in-memory `Limiter` key for this identity (per-IP daily bucket).
    pub fn limiter_key(&self) -> String {
        crate::state::ip_daily_key(&self.ip)
    }
}

impl FromRequestParts<AppState> for ClientIdentity {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0);
        Self::resolve(peer, &parts.headers, state.config.trust_proxy)
            .ok_or_else(|| AppError::Internal("client address unavailable".to_string()))
    }
}

/// Governor key extractor keyed by [`ClientIdentity`]: the same resolution
/// (and the same `TRUST_PROXY` flag) as handlers and the `Limiter`, so a
/// client never lands in different buckets across the two rate limiters.
#[derive(Debug, Clone, Copy)]
pub struct ClientIdentityExtractor {
    pub trust_proxy: bool,
}

impl ClientIdentityExtractor {
    pub fn new(trust_proxy: bool) -> Self {
        Self { trust_proxy }
    }
}

impl KeyExtractor for ClientIdentityExtractor {
    type Key = IpAddr;

    fn extract<T>(&self, req: &Request<T>) -> Result<IpAddr, GovernorError> {
        let peer = req
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0);
        ClientIdentity::resolve(peer, req.headers(), self.trust_proxy)
            .map(|id| id.ip())
            .ok_or(GovernorError::UnableToExtractKey)
    }
}

/// Leftmost valid entry wins: proxies append downstream, so the first entry
/// is the original client as seen by the trusted proxy.
fn maybe_x_forwarded_for(headers: &HeaderMap) -> Option<IpAddr> {
    headers
        .get("x-forwarded-for")
        .and_then(|hv| hv.to_str().ok())
        .and_then(|s| s.split(',').find_map(|e| e.trim().parse::<IpAddr>().ok()))
}

fn maybe_x_real_ip(headers: &HeaderMap) -> Option<IpAddr> {
    headers
        .get("x-real-ip")
        .and_then(|hv| hv.to_str().ok())
        .and_then(|s| s.trim().parse::<IpAddr>().ok())
}

/// First `Forwarded` element's `for=` identifier (RFC 7239). Only plain IPs
/// and bracketed IPv6 (with optional port) are honored; `unknown`,
/// obfuscated, and unparseable values fall through to the next source.
fn maybe_forwarded(headers: &HeaderMap) -> Option<IpAddr> {
    headers
        .get_all(axum::http::header::FORWARDED)
        .iter()
        .find_map(|hv| {
            let first = hv.to_str().ok()?.split(',').next()?;
            first.split(';').find_map(|pair| {
                let (key, value) = pair.split_once('=')?;
                if !key.trim().eq_ignore_ascii_case("for") {
                    return None;
                }
                parse_forwarded_for(value.trim())
            })
        })
}

fn parse_forwarded_for(value: &str) -> Option<IpAddr> {
    let unquoted = value
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(value);
    if let Some(rest) = unquoted.strip_prefix('[') {
        return rest.split(']').next()?.parse::<IpAddr>().ok();
    }
    if let Ok(ip) = unquoted.parse::<IpAddr>() {
        return Some(ip);
    }
    // Bare IPv4 with a port (`1.2.3.4:5678`); anything with more than one
    // colon is a bare IPv6 that already failed to parse — never strip it.
    let (host, port) = unquoted.rsplit_once(':')?;
    if !port.bytes().all(|b| b.is_ascii_digit()) || host.contains(':') {
        return None;
    }
    host.parse::<IpAddr>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn socket(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                k.parse::<axum::http::HeaderName>().unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    fn resolve(
        peer: &str,
        header_pairs: &[(&str, &str)],
        trust_proxy: bool,
    ) -> Option<ClientIdentity> {
        ClientIdentity::resolve(Some(socket(peer)), &headers(header_pairs), trust_proxy)
    }

    #[test]
    fn mapped_ipv4_normalizes_to_v4() {
        assert_eq!(
            ClientIdentity::normalize_ip("::ffff:1.2.3.4".parse().unwrap()),
            "1.2.3.4".parse::<IpAddr>().unwrap()
        );
        // Pure IPv6 and IPv4 pass through untouched.
        let v6: IpAddr = "2001:db8::1".parse().unwrap();
        assert_eq!(ClientIdentity::normalize_ip(v6), v6);
        let v4: IpAddr = "1.2.3.4".parse().unwrap();
        assert_eq!(ClientIdentity::normalize_ip(v4), v4);
    }

    #[test]
    fn limiter_key_and_hash_agree_across_mapped_and_v4() {
        let mapped = ClientIdentity::from_peer(socket("[::ffff:127.0.0.1]:54321"));
        let plain = ClientIdentity::from_peer(socket("127.0.0.1:54321"));
        assert_eq!(mapped, plain);
        assert_eq!(mapped.limiter_key(), plain.limiter_key());
        assert_eq!(
            crate::ip_hash::hash_ip(&mapped.ip(), Some("s")),
            crate::ip_hash::hash_ip(&plain.ip(), Some("s"))
        );
    }

    #[test]
    fn proxy_headers_ignored_when_trust_proxy_unset() {
        let id = resolve(
            "127.0.0.1:1",
            &[
                ("x-forwarded-for", "9.9.9.9"),
                ("x-real-ip", "8.8.8.8"),
                ("forwarded", "for=7.7.7.7"),
            ],
            false,
        )
        .unwrap();
        assert_eq!(id.ip(), "127.0.0.1".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn leftmost_xff_entry_wins_when_trust_proxy_set() {
        let id = resolve(
            "127.0.0.1:1",
            &[("x-forwarded-for", "9.9.9.9, 10.0.0.1, 172.16.0.1")],
            true,
        )
        .unwrap();
        assert_eq!(id.ip(), "9.9.9.9".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn header_precedence_is_xff_then_real_ip_then_forwarded_then_peer() {
        // X-Real-IP alone.
        let id = resolve("127.0.0.1:1", &[("x-real-ip", "8.8.8.8")], true).unwrap();
        assert_eq!(id.ip(), "8.8.8.8".parse::<IpAddr>().unwrap());
        // Forwarded alone.
        let id = resolve("127.0.0.1:1", &[("forwarded", "for=7.7.7.7")], true).unwrap();
        assert_eq!(id.ip(), "7.7.7.7".parse::<IpAddr>().unwrap());
        // XFF beats both.
        let id = resolve(
            "127.0.0.1:1",
            &[("x-forwarded-for", "9.9.9.9"), ("x-real-ip", "8.8.8.8")],
            true,
        )
        .unwrap();
        assert_eq!(id.ip(), "9.9.9.9".parse::<IpAddr>().unwrap());
        // Nothing present: the peer.
        let id = resolve("127.0.0.1:1", &[], true).unwrap();
        assert_eq!(id.ip(), "127.0.0.1".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn invalid_header_values_fall_back_to_peer() {
        for hdrs in [
            vec![("x-forwarded-for", "not-an-ip")],
            vec![("x-forwarded-for", "")],
            vec![("x-real-ip", "999.1.1.1")],
            vec![("forwarded", "for=unknown")],
            vec![("forwarded", "for=_hidden")],
            vec![("forwarded", "by=proxy;host=example.com")],
        ] {
            let id = resolve("127.0.0.1:1", &hdrs, true).unwrap();
            assert_eq!(
                id.ip(),
                "127.0.0.1".parse::<IpAddr>().unwrap(),
                "unusable headers must fall back to the peer: {hdrs:?}"
            );
        }
    }

    #[test]
    fn forwarded_supports_bracketed_ipv6_and_ports() {
        let id = resolve(
            "127.0.0.1:1",
            &[("forwarded", r#"for="[2001:db8::1]:1234""#)],
            true,
        )
        .unwrap();
        assert_eq!(id.ip(), "2001:db8::1".parse::<IpAddr>().unwrap());
        let id = resolve("127.0.0.1:1", &[("forwarded", "for=9.9.9.9:5678")], true).unwrap();
        assert_eq!(id.ip(), "9.9.9.9".parse::<IpAddr>().unwrap());
        // A bare IPv6 address is never port-stripped.
        let id = resolve("127.0.0.1:1", &[("forwarded", "for=2001:db8::1")], true).unwrap();
        assert_eq!(id.ip(), "2001:db8::1".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn mapped_header_value_normalizes() {
        let id = resolve(
            "127.0.0.1:1",
            &[("x-forwarded-for", "::ffff:9.9.9.9")],
            true,
        )
        .unwrap();
        assert_eq!(id.ip(), "9.9.9.9".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn no_peer_and_no_headers_resolves_to_none() {
        assert!(ClientIdentity::resolve(None, &headers(&[]), false).is_none());
        assert!(ClientIdentity::resolve(None, &headers(&[]), true).is_none());
    }

    #[test]
    fn extractor_ignores_spoofed_headers_when_unset() {
        let req = Request::builder()
            .header("x-forwarded-for", "9.9.9.9")
            .extension(ConnectInfo(socket("127.0.0.1:1")))
            .body(())
            .unwrap();
        let key = ClientIdentityExtractor::new(false).extract(&req).unwrap();
        assert_eq!(key, "127.0.0.1".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn extractor_honors_headers_when_set() {
        let req = Request::builder()
            .header("x-forwarded-for", "9.9.9.9")
            .extension(ConnectInfo(socket("127.0.0.1:1")))
            .body(())
            .unwrap();
        let key = ClientIdentityExtractor::new(true).extract(&req).unwrap();
        assert_eq!(key, "9.9.9.9".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn extractor_errors_without_any_address() {
        let req = Request::builder().body(()).unwrap();
        assert!(ClientIdentityExtractor::new(true).extract(&req).is_err());
    }
}
