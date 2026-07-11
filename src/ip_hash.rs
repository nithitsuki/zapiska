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
