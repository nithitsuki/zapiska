use sha2::{Digest, Sha256};

/// Hash an IP address with an optional secret salt.
/// Uses SHA-256, prefixed with "h:" to distinguish from raw IPs.
/// The salt prevents rainbow table attacks on the IP hash.
pub fn hash_ip(ip: &std::net::IpAddr, secret: Option<&str>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(ip.to_string().as_bytes());
    if let Some(s) = secret {
        hasher.update(s.as_bytes());
    }
    let result = hasher.finalize();
    let hex: String = result.iter().map(|b| format!("{:02x}", b)).collect();
    format!("h:{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(s: &str) -> std::net::IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn deterministic_for_same_input_and_secret() {
        assert_eq!(
            hash_ip(&v4("1.2.3.4"), Some("s3cr3t")),
            hash_ip(&v4("1.2.3.4"), Some("s3cr3t"))
        );
        assert_eq!(
            hash_ip(&v4("::1"), Some("s3cr3t")),
            hash_ip(&v4("::1"), Some("s3cr3t"))
        );
        assert_eq!(hash_ip(&v4("9.9.9.9"), None), hash_ip(&v4("9.9.9.9"), None));
    }

    #[test]
    fn secret_changes_output() {
        let salted = hash_ip(&v4("1.2.3.4"), Some("s3cr3t"));
        assert_ne!(salted, hash_ip(&v4("1.2.3.4"), None));
        assert_ne!(salted, hash_ip(&v4("1.2.3.4"), Some("other")));
    }

    #[test]
    fn output_has_h_prefix() {
        for (ip, secret) in [
            (v4("1.2.3.4"), None),
            (v4("1.2.3.4"), Some("s3cr3t")),
            (v4("::1"), None),
            (v4("2001:db8::1"), Some("s3cr3t")),
        ] {
            assert!(
                hash_ip(&ip, secret).starts_with("h:"),
                "hash must be prefixed with h:"
            );
        }
    }

    #[test]
    fn output_is_64_lowercase_hex_chars() {
        for (ip, secret) in [
            (v4("1.2.3.4"), None),
            (v4("1.2.3.4"), Some("s3cr3t")),
            (v4("::1"), Some("s3cr3t")),
        ] {
            let out = hash_ip(&ip, secret);
            let hex = out.strip_prefix("h:").expect("h: prefix");
            assert_eq!(hex.len(), 64, "SHA-256 hex digest length: {out}");
            assert!(
                hex.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
                "lowercase hex only: {out}"
            );
        }
    }

    #[test]
    fn handles_ipv4_and_ipv6() {
        let v4_hash = hash_ip(&v4("1.2.3.4"), Some("s3cr3t"));
        let v6_hash = hash_ip(&v4("::1"), Some("s3cr3t"));
        let v6_full = hash_ip(&v4("2001:db8::1"), None);
        for h in [&v4_hash, &v6_hash, &v6_full] {
            assert!(h.starts_with("h:") && h.len() == 66, "well-formed: {h}");
        }
        assert_ne!(v4_hash, v6_hash, "distinct inputs hash distinctly");
    }

    #[test]
    fn empty_secret_matches_none() {
        // Config::from_env drops empty IP_HASH_SECRET, and an empty salt
        // contributes zero bytes, so Some("") must equal None. Pinned so a
        // future change to empty handling is a deliberate, visible break.
        assert_eq!(
            hash_ip(&v4("1.2.3.4"), Some("")),
            hash_ip(&v4("1.2.3.4"), None)
        );
    }

    #[test]
    fn stability_vectors() {
        // Locks the output for (input, secret): any algorithm change
        // (hash fn, encoding, salt placement) fails loudly here and in the
        // v7 backfill parity test in src/db/pool.rs.
        // Vectors anchor "h:" + hex(SHA256(ip.to_string() || secret)), independently cross-checked against Python hashlib — not generated from this code.
        assert_eq!(
            hash_ip(&v4("1.2.3.4"), None),
            "h:6694f83c9f476da31f5df6bcc520034e7e57d421d247b9d34f49edbfc84a764c"
        );
        assert_eq!(
            hash_ip(&v4("1.2.3.4"), Some("test-secret")),
            "h:05991cde7f313a8ba877f08b622a73ff5a9099066b641659621938a88a82534a"
        );
        assert_eq!(
            hash_ip(&v4("::1"), Some("test-secret")),
            "h:f9881e07616f7b6d4b93d0e328e20141056a81f37d4ecba5970061c699ba2ebe"
        );
        assert_eq!(
            hash_ip(&v4("2001:db8::1"), None),
            "h:5afd19e856d1c18d17d600dfd2b5f534992333985e126c2a951047102c1ed536"
        );
    }
}
