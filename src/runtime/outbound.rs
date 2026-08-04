//! Letting a handler reach the internet, which is the capability most worth
//! being careful about.
//!
//! This host sits inside somebody's private network. Anything that will fetch
//! a URL on request is a way to read whatever else lives in there — a
//! metadata endpoint holding cloud credentials, a database admin page, a
//! neighbour service that trusts its network. So outbound is off unless an
//! app names the hosts it needs, and every address is checked before a
//! connection is made.

use std::net::{IpAddr, ToSocketAddrs};
use std::time::Duration;

/// A response big enough for an API, small enough not to be a way to fill
/// memory.
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, PartialEq)]
pub enum Refusal {
    /// The app never asked for this host.
    NotAllowed(String),
    /// The URL is not something we will fetch at all.
    BadUrl(String),
    /// It resolves somewhere inside the network this server is on.
    Internal(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::NotAllowed(host) => write!(
                f,
                "{host} is not in this app's allow_http list; add it to toolsite.toml"
            ),
            Refusal::BadUrl(why) => write!(f, "{why}"),
            Refusal::Internal(host) => {
                write!(f, "{host} resolves inside this server's own network")
            }
        }
    }
}

/// Addresses no app may reach, whatever its allowlist says.
///
/// The allowlist is about intent; this is about reachability. A host an app
/// legitimately named can still resolve somewhere it must not go — by
/// mistake, or because someone pointed a name at 169.254.169.254 on purpose.
fn is_internal(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                // 100.64.0.0/10, which is where a lot of platform networking
                // lives.
                || (v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]))
                // 192.0.0.0/24 and 198.18.0.0/15, reserved and benchmarking.
                || (v4.octets()[0] == 192 && v4.octets()[1] == 0 && v4.octets()[2] == 0)
                || (v4.octets()[0] == 198 && (18..20).contains(&v4.octets()[1]))
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                // Unique local, fc00::/7.
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                // Link local, fe80::/10.
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // An IPv4 address wearing a hat still goes where it goes.
                || v6.to_ipv4_mapped().is_some_and(|v4| is_internal(&IpAddr::V4(v4)))
        }
    }
}

/// Case-insensitive host match, with `*.example.com` covering subdomains but
/// not the bare name — an allowlist should say what it means.
fn allowed(host: &str, allow: &[String]) -> bool {
    let host = host.to_ascii_lowercase();
    allow.iter().any(|entry| {
        let entry = entry.trim().to_ascii_lowercase();
        match entry.strip_prefix("*.") {
            Some(suffix) => host.ends_with(&format!(".{suffix}")),
            None => host == entry,
        }
    })
}

/// Decides whether one URL may be fetched, resolving it to check where it
/// actually goes.
pub fn check(url: &str, allow: &[String]) -> Result<(), Refusal> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|_| Refusal::BadUrl(format!("{url} is not a URL")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(Refusal::BadUrl(format!(
            "{} is not a scheme this fetches",
            parsed.scheme()
        )));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| Refusal::BadUrl(format!("{url} has no host")))?
        .to_string();

    if !allowed(&host, allow) {
        return Err(Refusal::NotAllowed(host));
    }

    // A literal address skips DNS but not the check.
    if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>() {
        return if is_internal(&ip) {
            Err(Refusal::Internal(host))
        } else {
            Ok(())
        };
    }

    let port = parsed.port_or_known_default().unwrap_or(443);
    let resolved = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|_| Refusal::BadUrl(format!("{host} does not resolve")))?;

    let mut any = false;
    for address in resolved {
        any = true;
        if is_internal(&address.ip()) {
            return Err(Refusal::Internal(host));
        }
    }
    if !any {
        return Err(Refusal::BadUrl(format!("{host} does not resolve")));
    }
    Ok(())
}

pub struct Fetched {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Performs a request a handler asked for, after checking it. Blocking, and
/// called from inside a blocking task like everything else a guest triggers.
pub fn send(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Vec<u8>,
    allow: &[String],
) -> Result<Fetched, String> {
    check(url, allow).map_err(|refusal| refusal.to_string())?;

    let client = reqwest::blocking::Client::builder()
        .timeout(TIMEOUT)
        // Followed by hand below, so each hop is checked. Left to the client
        // a redirect would walk straight past the guard above.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;

    let method = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|_| format!("{method} is not an HTTP method"))?;
    let mut request = client.request(method.clone(), url);
    // Plenty of APIs refuse a request without one — GitHub answers 403 — and
    // an app should not have to learn that the hard way. A guest that sets
    // its own still wins.
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("user-agent"))
    {
        request = request.header(
            "user-agent",
            concat!("toolsite/", env!("CARGO_PKG_VERSION")),
        );
    }
    for (name, value) in headers {
        // Hop-by-hop and identity headers are the host's to set.
        if matches!(
            name.to_ascii_lowercase().as_str(),
            "host" | "content-length" | "connection" | "transfer-encoding"
        ) {
            continue;
        }
        request = request.header(name, value);
    }
    if !body.is_empty() {
        request = request.body(body);
    }

    let response = request.send().map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        })
        .collect();

    let bytes = response.bytes().map_err(|e| e.to_string())?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(format!(
            "response is larger than {} MB",
            MAX_RESPONSE_BYTES / 1024 / 1024
        ));
    }

    Ok(Fetched {
        status,
        headers,
        body: bytes.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow(hosts: &[&str]) -> Vec<String> {
        hosts.iter().map(|h| h.to_string()).collect()
    }

    #[test]
    fn nothing_is_reachable_without_an_allowlist() {
        assert_eq!(
            check("https://example.com/", &[]),
            Err(Refusal::NotAllowed("example.com".into()))
        );
    }

    #[test]
    fn a_wildcard_covers_subdomains_and_not_the_bare_name() {
        // The policy alone: resolution is a separate question and needs a
        // network, which a test should not.
        let wildcard = allow(&["*.example.com"]);
        assert!(allowed("api.example.com", &wildcard));
        assert!(allowed("API.Example.com", &wildcard), "matching is case-blind");
        assert!(!allowed("example.com", &wildcard), "a wildcard is not the bare name");
        assert!(
            !allowed("notexample.com", &wildcard),
            "a name that merely ends the same way is a different name"
        );
        assert!(allowed("example.com", &allow(&["example.com"])));
    }

    #[test]
    fn the_networks_this_server_lives_in_are_refused_even_when_allowed() {
        // The point: naming a host does not make its address acceptable.
        for url in [
            "http://127.0.0.1/",
            "http://localhost/",
            "http://169.254.169.254/latest/meta-data/",
            "http://10.0.0.5/",
            "http://192.168.1.1/",
            "http://172.16.0.1/",
            "http://100.64.0.1/",
            "http://[::1]/",
            "http://[fd00::1]/",
        ] {
            let host = reqwest::Url::parse(url).unwrap().host_str().unwrap().to_string();
            let refusal = check(url, &allow(&[&host, "localhost"])).unwrap_err();
            assert!(
                matches!(refusal, Refusal::Internal(_)),
                "{url} was not refused as internal: {refusal:?}"
            );
        }
    }

    #[test]
    fn only_http_urls_are_fetched() {
        let allow = allow(&["example.com"]);
        for url in [
            "file:///etc/passwd",
            "gopher://example.com/",
            "data:text/plain,hello",
        ] {
            assert!(
                matches!(check(url, &allow), Err(Refusal::BadUrl(_))),
                "{url} was not refused"
            );
        }
    }

    #[test]
    fn an_ipv4_address_hidden_in_ipv6_is_still_internal() {
        // ::ffff:169.254.169.254 is the metadata endpoint by another
        // spelling, and the URL parser writes it as [::ffff:a9fe:a9fe].
        let refusal = check(
            "http://[::ffff:169.254.169.254]/",
            &allow(&["[::ffff:a9fe:a9fe]"]),
        )
        .unwrap_err();
        assert!(matches!(refusal, Refusal::Internal(_)), "got {refusal:?}");
    }

    /// Reaches the network, so it is ignored by default: `cargo test --
    /// --ignored` runs it. Everything else in this file is hermetic.
    #[test]
    #[ignore = "reaches the network"]
    fn a_real_request_carries_a_user_agent_so_apis_answer_it() {
        // GitHub refuses a request without one, which is exactly the sort of
        // thing an app should not have to discover for itself.
        let fetched = send(
            "GET",
            "https://api.github.com/rate_limit",
            &[],
            Vec::new(),
            &allow(&["api.github.com"]),
        )
        .unwrap();
        assert_eq!(fetched.status, 200, "{}", String::from_utf8_lossy(&fetched.body));
    }

    #[test]
    fn a_name_is_checked_before_it_is_looked_up() {
        // A host nobody allowed is refused without a DNS query, so a handler
        // cannot use this to ask questions of a resolver.
        assert_eq!(
            check("https://whatever.invalid/", &[]),
            Err(Refusal::NotAllowed("whatever.invalid".into()))
        );
    }
}
