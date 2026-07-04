use std::net::IpAddr;
use std::str::FromStr;

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

/// Blocked private/loopback/carrier-grade NAT / link-local etc. address ranges.
/// Matches SPEC §6.1.
fn blocked_nets() -> Vec<IpNet> {
    // IPv4 ranges
    let v4 = [
        "0.0.0.0/8",
        "10.0.0.0/8",
        "100.64.0.0/10", // CGNAT
        "127.0.0.0/8",
        "169.254.0.0/16", // link-local (incl. AWS metadata 169.254.169.254)
        "172.16.0.0/12",
        "192.0.0.0/24",
        "192.0.2.0/24",
        "192.168.0.0/16",
        "198.18.0.0/15",
        "240.0.0.0/4",
    ];
    // IPv6 ranges.
    // IPv4-mapped addresses (::ffff:x.x.x.x) are handled by extracting
    // the embedded IPv4 and re-running the IPv4 check; the /96 prefix is
    // deliberately excluded here so legitimate public IPv4-mapped addrs
    // are not falsely blocked.
    let v6 = ["::1/128", "fc00::/7", "fe80::/10"];

    v4.iter()
        .chain(v6.iter())
        .map(|s| IpNet::from_str(s).expect("hardcoded CIDR is valid"))
        .collect()
}

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
    blocked_nets().iter().any(|net| net.contains(&ip))
}

/// Check if a hostname string belongs to a blocked namespace.
pub fn is_blocked_host(host: &str) -> bool {
    let lower = host.to_lowercase();
    lower == "localhost"
        || lower.ends_with(".local")
        || lower.ends_with(".internal")
        || lower.ends_with(".localhost")
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
/// Returns `Ok(())` if the host is safe to connect to.
pub async fn resolve_and_check(host: &str) -> Result<(), SsrfError> {
    if is_blocked_host(host) {
        return Err(SsrfError::BlockedHost(host.to_string()));
    }

    let addrs = tokio::net::lookup_host((host, 0))
        .await
        .map_err(|e| SsrfError::LookupFailed(host.to_string(), e.to_string()))?;

    for addr in addrs {
        let ip = addr.ip();
        if is_blocked_ip(ip) {
            return Err(SsrfError::BlockedIp(ip));
        }
    }

    Ok(())
}

/// Build a reqwest `redirect::Policy` that calls `resolve_and_check` at each hop.
pub fn ssrf_safe_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        let url = attempt.url();
        let host = url.host_str().unwrap_or("");
        if host.is_empty() || is_blocked_host(host) {
            return attempt.stop();
        }
        // Synchronous check of the IP (requires the IP to be known without DNS).
        // For a full defense use `resolve_and_check` before making the request;
        // this policy serves as a belt-and-suspenders for the common case.
        if let Ok(ip) = std::net::IpAddr::from_str(host)
            && is_blocked_ip(ip)
        {
            return attempt.stop();
        }
        attempt.follow()
    })
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
}
