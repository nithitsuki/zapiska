use std::net::IpAddr;
use std::str::FromStr;
use std::sync::LazyLock;

use ipnet::IpNet;

#[derive(Debug, thiserror::Error)]
pub enum SsrfError {
    #[error("blocked hostname: {0}")]
    BlockedHost(String),
    #[error("blocked IP: {0}")]
    BlockedIp(IpAddr),
    #[error("DNS lookup failed for {0}: {1}")]
    LookupFailed(String, String),
}

/// Blocked private/loopback/CGNAT/link-local address ranges. Matches SPEC §6.1.
static BLOCKED_NETS: LazyLock<Vec<IpNet>> = LazyLock::new(|| {
    let v4 = [
        "0.0.0.0/8",
        "10.0.0.0/8",
        "100.64.0.0/10",
        "127.0.0.0/8",
        "169.254.0.0/16",
        "172.16.0.0/12",
        "192.0.0.0/24",
        "192.0.2.0/24",
        "192.168.0.0/16",
        "198.18.0.0/15",
        // T05: TEST-NET-2/3 (documentation, RFC 5737) — same class as the
        // already-blocked 192.0.2.0/24; omitted before, closed here.
        "198.51.100.0/24",
        "203.0.113.0/24",
        "240.0.0.0/4",
    ];
    // T05: added "::/128" (unspecified) and "2001:db8::/32" (documentation,
    // RFC 3849). Deliberately DEFERRED (see follow-up note on
    // `is_blocked_ip`): NAT64 "64:ff9b::/96", 6to4 "2002::/16", and Teredo
    // "2001::/32" with embedded private IPv4, plus zone-ID literals
    // ("[fe80::1%eth0]") — these need embedded-IPv4 re-checks and scoped
    // connect handling that belong in a dedicated pass, not this hotfix.
    let v6 = [
        "::/128",
        "::1/128",
        "fc00::/7",
        "fe80::/10",
        "2001:db8::/32",
    ];
    v4.iter()
        .chain(v6.iter())
        .map(|s| IpNet::from_str(s).expect("hardcoded CIDR is valid"))
        .collect()
});

/// Check if `ip` falls in any blocked range.
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    // For IPv4-mapped IPv6 addresses, extract and re-check the embedded IPv4.
    if let IpAddr::V6(v6) = ip
        && let Some(v4) = v6.to_ipv4_mapped()
        && is_blocked_ip_inner(IpAddr::V4(v4))
    {
        return true;
    }
    is_blocked_ip_inner(ip)
}

fn is_blocked_ip_inner(ip: IpAddr) -> bool {
    BLOCKED_NETS.iter().any(|net| net.contains(&ip))
}

/// Normalize a hostname for blocklist checks: lowercase and strip trailing
/// DNS dots, so `http://localhost./` cannot dodge the string checks that
/// `http://localhost/` hits. WHATWG URL parsing already normalizes numeric
/// IP forms (`127.1`, `0x7f000001`) to dotted quads in `host_str()`, so the
/// caller should always pass the serialized host, not the raw input.
pub fn normalize_host(host: &str) -> String {
    host.trim_end_matches('.').to_lowercase()
}

/// Check if a hostname string belongs to a blocked namespace.
pub fn is_blocked_host(host: &str) -> bool {
    let lower = normalize_host(host);
    lower == "localhost"
        || lower.ends_with(".local")
        || lower.ends_with(".internal")
        || lower.ends_with(".localhost")
}

/// Check if a host string is a literal loopback address (IPv4 127.x.x.x or
/// IPv6 ::1), tolerating a trailing DNS dot and IPv6 brackets. Used by the
/// fetcher so `allow_loopback` test plumbing keeps working per hop without
/// reopening named-host bypasses (`localhost` is NOT loopback here — it must
/// still go through DNS resolution and the blocklist).
pub fn is_loopback_host(host: &str) -> bool {
    let bare = host.trim_end_matches('.');
    let bare = bare
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(bare);
    if bare == "::1" {
        return true;
    }
    if let Ok(ip) = std::net::IpAddr::from_str(bare) {
        return ip.is_loopback();
    }
    false
}

/// Extract the registrable (eTLD+1) domain from a hostname.
///
/// Uses a simple public-suffix heuristic: the last two labels for common TLDs
/// and .com / .org / .net etc., or the last label for bare TLDs.
/// This is NOT a full PSL implementation — it's good enough for display
/// fallback when scraping an author's page.
pub fn registrable_domain(host: &str) -> String {
    let lower = host.to_lowercase();
    let labels: Vec<&str> = lower.split('.').collect();

    // Known two-part TLDs (non-exhaustive — extend as needed).
    const TWO_PART_TLDS: &[&str] = &[
        "co.uk", "org.uk", "ac.uk", "gov.uk", "net.uk", "nhs.uk", "com.au", "net.au", "org.au",
        "gov.au", "co.jp", "ne.jp", "or.jp", "co.nz", "net.nz", "org.nz", "co.kr", "or.kr",
        "ne.kr", "com.br", "org.br", "net.br", "gov.br", "co.in", "net.in", "org.in", "gen.in",
        "firm.in", "ind.in", "com.cn", "net.cn", "org.cn", "gov.cn", "co.za", "org.za", "net.za",
        "gov.za", "com.mx", "org.mx", "net.mx", "gob.mx",
    ];

    if labels.len() < 2 {
        return lower;
    }

    // Check if the last two labels form a two-part TLD.
    let last_two = labels[labels.len() - 2..].join(".");
    for tld in TWO_PART_TLDS {
        if last_two == *tld && labels.len() >= 3 {
            // e.g. "www.example.co.uk" -> "example.co.uk"
            return labels[labels.len() - 3..].join(".");
        }
    }

    // Default: last two labels.
    labels[labels.len() - 2..].join(".")
}

/// Resolve `host` to IP addresses and block if any fall into a private range.
/// Returns `Ok(())` if the host is safe to connect to. The host is
/// normalized first (trailing DNS dot stripped), so `localhost.` resolves
/// and checks exactly like `localhost`.
pub async fn resolve_and_check(host: &str) -> Result<(), SsrfError> {
    let normalized = normalize_host(host);
    if is_blocked_host(&normalized) {
        return Err(SsrfError::BlockedHost(normalized));
    }

    let addrs = tokio::net::lookup_host((normalized.as_str(), 0))
        .await
        .map_err(|e| SsrfError::LookupFailed(normalized.clone(), e.to_string()))?;

    for addr in addrs {
        let ip = addr.ip();
        if is_blocked_ip(ip) {
            return Err(SsrfError::BlockedIp(ip));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::net::Ipv6Addr;

    // ── IPv4 blocklist ───────────────────────────────────

    #[test]
    fn zero_zero_zero_zero_blocked() {
        assert!(is_blocked_ip(Ipv4Addr::new(0, 0, 0, 0).into()));
        assert!(is_blocked_ip(Ipv4Addr::new(0, 255, 255, 255).into()));
    }

    #[test]
    fn ten_range_blocked() {
        assert!(is_blocked_ip(Ipv4Addr::new(10, 0, 0, 1).into()));
        assert!(is_blocked_ip(Ipv4Addr::new(10, 255, 255, 255).into()));
    }

    #[test]
    fn cgnat_100_64_blocked() {
        assert!(is_blocked_ip(Ipv4Addr::new(100, 64, 0, 1).into()));
        assert!(is_blocked_ip(Ipv4Addr::new(100, 127, 255, 255).into()));
        // 100.128.0.0 is outside CGNAT range
        assert!(!is_blocked_ip(Ipv4Addr::new(100, 128, 0, 1).into()));
    }

    #[test]
    fn loopback_127_blocked() {
        assert!(is_blocked_ip(Ipv4Addr::new(127, 0, 0, 1).into()));
        assert!(is_blocked_ip(Ipv4Addr::new(127, 255, 255, 255).into()));
    }

    #[test]
    fn linklocal_169_254_blocked() {
        assert!(is_blocked_ip(Ipv4Addr::new(169, 254, 169, 254).into()));
        assert!(is_blocked_ip(Ipv4Addr::new(169, 254, 0, 1).into()));
        assert!(is_blocked_ip(Ipv4Addr::new(169, 254, 255, 255).into()));
    }

    #[test]
    fn docker_172_16_blocked() {
        assert!(is_blocked_ip(Ipv4Addr::new(172, 16, 0, 1).into()));
        assert!(is_blocked_ip(Ipv4Addr::new(172, 31, 255, 255).into()));
        // 172.32.0.0 is outside
        assert!(!is_blocked_ip(Ipv4Addr::new(172, 32, 0, 1).into()));
    }

    #[test]
    fn private_192_168_blocked() {
        assert!(is_blocked_ip(Ipv4Addr::new(192, 168, 0, 1).into()));
        assert!(is_blocked_ip(Ipv4Addr::new(192, 168, 255, 255).into()));
    }

    #[test]
    fn benchmark_special_192_0_blocked() {
        assert!(is_blocked_ip(Ipv4Addr::new(192, 0, 0, 1).into()));
        assert!(is_blocked_ip(Ipv4Addr::new(192, 0, 0, 255).into()));
    }

    #[test]
    fn documentation_192_0_2_blocked() {
        assert!(is_blocked_ip(Ipv4Addr::new(192, 0, 2, 1).into()));
        assert!(is_blocked_ip(Ipv4Addr::new(192, 0, 2, 255).into()));
    }

    #[test]
    fn benchmark_198_18_blocked() {
        assert!(is_blocked_ip(Ipv4Addr::new(198, 18, 0, 1).into()));
        assert!(is_blocked_ip(Ipv4Addr::new(198, 19, 255, 255).into()));
    }

    #[test]
    fn multicast_240_blocked() {
        assert!(is_blocked_ip(Ipv4Addr::new(240, 0, 0, 1).into()));
        assert!(is_blocked_ip(Ipv4Addr::new(255, 255, 255, 255).into()));
    }

    // ── Public IPv4 allowed ──────────────────────────────

    #[test]
    fn public_dns_allowed() {
        assert!(!is_blocked_ip(Ipv4Addr::new(8, 8, 8, 8).into()));
        assert!(!is_blocked_ip(Ipv4Addr::new(1, 1, 1, 1).into()));
    }

    // ── IPv6 blocklist ───────────────────────────────────

    #[test]
    fn ipv6_loopback_blocked() {
        let v6: Ipv6Addr = "::1".parse().unwrap();
        assert!(is_blocked_ip(v6.into()));
    }

    #[test]
    fn ipv6_unique_local_blocked() {
        let v6: Ipv6Addr = "fc00::1".parse().unwrap();
        assert!(is_blocked_ip(v6.into()));
        let v6: Ipv6Addr = "fdff::1".parse().unwrap();
        assert!(is_blocked_ip(v6.into()));
    }

    #[test]
    fn ipv6_linklocal_blocked() {
        let v6: Ipv6Addr = "fe80::1".parse().unwrap();
        assert!(is_blocked_ip(v6.into()));
        let v6: Ipv6Addr = "febf::1".parse().unwrap();
        assert!(is_blocked_ip(v6.into()));
    }

    // ── IPv4-mapped IPv6 ─────────────────────────────────

    #[test]
    fn ipv4_mapped_10_0_0_1_blocked() {
        let v6 = Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0x0a00, 0x0001);
        assert!(
            is_blocked_ip(v6.into()),
            "::ffff:10.0.0.1 should be blocked"
        );
    }

    #[test]
    fn ipv4_mapped_8_8_8_8_allowed() {
        let v6 = Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0x0808, 0x0808);
        assert!(
            !is_blocked_ip(v6.into()),
            "::ffff:8.8.8.8 should be allowed"
        );
    }

    // ── Host strings ─────────────────────────────────────

    #[test]
    fn localhost_string_blocked() {
        assert!(is_blocked_host("localhost"));
        assert!(is_blocked_host("LOCALHOST"));
    }

    #[test]
    fn dot_local_blocked() {
        assert!(is_blocked_host("host.local"));
        assert!(is_blocked_host("foo.bar.local"));
    }

    #[test]
    fn dot_internal_blocked() {
        assert!(is_blocked_host("db.internal"));
        assert!(is_blocked_host("secret.db.internal"));
    }

    #[test]
    fn dot_localhost_blocked() {
        assert!(is_blocked_host("dev.localhost"));
        assert!(is_blocked_host("app.dev.localhost"));
    }

    #[test]
    fn public_host_allowed() {
        assert!(!is_blocked_host("example.com"));
        assert!(!is_blocked_host("alice.blog"));
    }

    // ── Registrable domain ───────────────────────────────

    #[test]
    fn regdomain_simple_com() {
        assert_eq!(registrable_domain("example.com"), "example.com");
        assert_eq!(registrable_domain("www.example.com"), "example.com");
    }

    #[test]
    fn regdomain_two_part_tld() {
        assert_eq!(registrable_domain("example.co.uk"), "example.co.uk");
        assert_eq!(registrable_domain("www.example.co.uk"), "example.co.uk");
        assert_eq!(
            registrable_domain("deep.www.example.co.uk"),
            "example.co.uk"
        );
    }

    #[test]
    fn regdomain_bare_host() {
        assert_eq!(registrable_domain("localhost"), "localhost");
        assert_eq!(registrable_domain("my-dev-box"), "my-dev-box");
    }

    #[test]
    fn regdomain_case_insensitive() {
        assert_eq!(registrable_domain("WWW.EXAMPLE.COM"), "example.com");
    }

    #[test]
    fn regdomain_australia() {
        assert_eq!(registrable_domain("blog.example.com.au"), "example.com.au");
    }

    #[test]
    fn regdomain_japan() {
        assert_eq!(registrable_domain("site.example.co.jp"), "example.co.jp");
    }

    // ── Edge cases ───────────────────────────────────────

    #[test]
    fn wildcard_localhost_string() {
        assert!(is_blocked_host("anything.localhost"));
    }

    // ── T05 gap-closure table (must fail before the fix) ───

    #[test]
    fn t05_trailing_dot_blocked() {
        // `http://localhost./` must not dodge the hostname check.
        assert!(is_blocked_host("localhost."));
        assert!(is_blocked_host("LOCALHOST."));
        assert!(is_blocked_host("db.internal."));
        assert!(is_blocked_host("host.local."));
        assert_eq!(normalize_host("Example.COM."), "example.com");
    }

    #[test]
    fn t05_test_net_2_and_3_blocked() {
        // TEST-NET-2/3 (documentation) were missing beside 192.0.2.0/24.
        assert!(is_blocked_ip(Ipv4Addr::new(198, 51, 100, 1).into()));
        assert!(is_blocked_ip(Ipv4Addr::new(203, 0, 113, 1).into()));
    }

    #[test]
    fn t05_ipv6_unspecified_and_documentation_blocked() {
        let unspec: Ipv6Addr = "::".parse().unwrap();
        assert!(is_blocked_ip(unspec.into()));
        let doc: Ipv6Addr = "2001:db8::1".parse().unwrap();
        assert!(is_blocked_ip(doc.into()));
    }

    #[test]
    fn t05_loopback_host_helper() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("127.0.0.1."));
        assert!(is_loopback_host("::1"));
        assert!(!is_loopback_host("8.8.8.8"));
        assert!(!is_loopback_host("localhost"));
    }
}
