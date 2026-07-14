//! SSRF-safe URL validation for server-side fetches.
//!
//! Blocks private/loopback/link-local/metadata IP ranges and non-http(s) schemes
//! before the server (image proxy, vision captioning, custom engines) opens an
//! outbound connection. Hostnames that are not IP literals are allowed through
//! (DNS rebinding to private IPs after redirect is mitigated by a custom
//! redirect policy on sensitive fetches).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use url::Url;

/// Build an HTTP client for SSRF-sensitive fetches (image proxy, vision caption).
/// Redirect hops are validated with [`is_safe_public_url`].
pub fn safe_fetch_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(safe_redirect_policy())
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Returns `true` when `raw` is an `http://` or `https://` URL whose host is
/// not a blocked private/loopback/link-local/metadata address.
pub fn is_safe_public_url(raw: &str) -> bool {
    let url = match Url::parse(raw.trim()) {
        Ok(u) => u,
        Err(_) => return false,
    };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    let host = match url.host() {
        Some(url::Host::Domain(h)) if !h.is_empty() => h.to_string(),
        Some(url::Host::Ipv4(ip)) => return !is_blocked_ip(IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => return !is_blocked_ip(IpAddr::V6(ip)),
        _ => return false,
    };
    if is_blocked_hostname(&host) {
        return false;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return !is_blocked_ip(ip);
    }
    if looks_like_ipv4(&host) {
        if let Some(ip) = parse_dotted_ipv4(&host) {
            return !is_blocked_ip(ip);
        }
        return false;
    }
    true
}

/// `reqwest` redirect policy: follow only when the next hop is also safe.
pub fn safe_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if is_safe_public_url(attempt.url().as_str()) {
            attempt.follow()
        } else {
            attempt.stop()
        }
    })
}

fn is_blocked_hostname(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    matches!(
        h.as_str(),
        "localhost"
            | "localhost.localdomain"
            | "metadata.google.internal"
            | "metadata"
            | "instance-data"
    ) || h.ends_with(".localhost")
        || h.ends_with(".local")
        || h.ends_with(".internal")
}

fn looks_like_ipv4(host: &str) -> bool {
    host.chars().all(|c| c.is_ascii_digit() || c == '.') && host.contains('.')
}

fn parse_dotted_ipv4(host: &str) -> Option<IpAddr> {
    let mut octets = [0u8; 4];
    for (i, part) in host.split('.').enumerate() {
        if i >= 4 {
            return None;
        }
        octets[i] = part.parse().ok()?;
    }
    Some(IpAddr::V4(Ipv4Addr::from(octets)))
}

/// Block RFC1918, loopback, link-local, ULA, and cloud metadata ranges.
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_ipv4(v4),
        IpAddr::V6(v6) => is_blocked_ipv6(v6),
    }
}

fn is_blocked_ipv4(ip: Ipv4Addr) -> bool {
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.octets()[0] == 0
        // 169.254.0.0/16 (link-local / AWS/GCP metadata)
        || (ip.octets()[0] == 169 && ip.octets()[1] == 254)
        // 100.64.0.0/10 CGNAT
        || (ip.octets()[0] == 100 && (ip.octets()[1] & 0b1100_0000) == 0b0100_0000)
}

fn is_blocked_ipv6(ip: Ipv6Addr) -> bool {
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        // Unique local fc00::/7
        || (ip.segments()[0] & 0xfe00) == 0xfc00
        // Link-local fe80::/10
        || (ip.segments()[0] & 0xffc0) == 0xfe80
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_public_https() {
        assert!(is_safe_public_url("https://example.com/path"));
        assert!(is_safe_public_url("http://rust-lang.org/"));
    }

    #[test]
    fn blocks_private_and_metadata_ips() {
        assert!(!is_safe_public_url("http://127.0.0.1/"));
        assert!(!is_safe_public_url("http://127.0.0.1:8080/admin"));
        assert!(!is_safe_public_url("http://10.0.0.1/"));
        assert!(!is_safe_public_url("http://172.16.0.1/"));
        assert!(!is_safe_public_url("http://192.168.1.1/"));
        assert!(!is_safe_public_url(
            "http://169.254.169.254/latest/meta-data/"
        ));
        assert!(!is_safe_public_url("http://[::1]/"));
        assert!(!is_safe_public_url("http://[fc00::1]/"));
    }

    #[test]
    fn blocks_localhost_hostnames() {
        assert!(!is_safe_public_url("http://localhost/"));
        assert!(!is_safe_public_url("http://metadata.google.internal/"));
    }

    #[test]
    fn blocks_non_http_schemes() {
        assert!(!is_safe_public_url("file:///etc/passwd"));
        assert!(!is_safe_public_url("ftp://example.com/"));
        assert!(!is_safe_public_url("gopher://example.com/"));
    }

    #[test]
    fn blocked_ip_ranges() {
        assert!(is_blocked_ip("127.0.0.1".parse().unwrap()));
        assert!(is_blocked_ip("10.1.2.3".parse().unwrap()));
        assert!(is_blocked_ip("169.254.169.254".parse().unwrap()));
        assert!(!is_blocked_ip("8.8.8.8".parse().unwrap()));
        assert!(!is_blocked_ip("1.1.1.1".parse().unwrap()));
    }
}
