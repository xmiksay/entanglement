//! The SSRF guard behind `web_fetch` (#736, ADR-0220): which addresses a
//! model-supplied URL may reach, enforced in two places that together leave
//! no unchecked path to a socket:
//!
//! - [`validate_url`] runs on the initial URL and on **every redirect
//!   target** before a request is built: scheme, embedded credentials, an
//!   IP-literal host (reqwest never consults a resolver for one), and
//!   `localhost`-style names.
//! - [`GuardedResolver`] is the client's DNS resolver, so the addresses it
//!   vets are exactly the addresses the connector dials — there is no second
//!   lookup an attacker's DNS could answer differently (DNS rebinding).
//!
//! The blocklist is deliberately broad (everything that isn't plainly a
//! public unicast address), and IPv6 forms that embed an IPv4 address are
//! unwrapped and judged by the embedded address, so `[::ffff:127.0.0.1]` is
//! as dead as `127.0.0.1`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use anyhow::{bail, Result};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::Url;

/// Which addresses a fetch may reach. Production builds have exactly one
/// policy (block every non-public address); the `cfg(test)` field lets this
/// crate's own tests reach their `127.0.0.1` fixture servers without any
/// runtime-reachable bypass existing outside a test build.
#[derive(Clone, Default)]
pub(super) struct Guard {
    #[cfg(test)]
    pub(super) allow_loopback: bool,
    /// Fake DNS answers consulted before the real resolver — how a test
    /// makes a public-looking name resolve to a private address.
    #[cfg(test)]
    pub(super) fake_dns: Vec<(String, IpAddr)>,
}

impl Guard {
    pub(super) fn permits(&self, ip: IpAddr) -> bool {
        #[cfg(test)]
        if self.allow_loopback && ip.is_loopback() {
            return true;
        }
        !is_blocked(ip)
    }
}

/// Reject a URL the fetcher must never request: a non-http(s) scheme,
/// userinfo credentials (they would be sent to whatever host the URL names
/// — and a redirect can name any host), a blocked IP literal, or a
/// `localhost`-style name. Domain names are otherwise left to
/// [`GuardedResolver`], which sees the real resolved addresses.
pub(super) fn validate_url(url: &Url, guard: &Guard) -> Result<()> {
    if !matches!(url.scheme(), "http" | "https") {
        bail!(
            "only http and https URLs can be fetched (got `{}`)",
            url.scheme()
        );
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("URLs with embedded credentials are refused");
    }
    let Some(host) = url.host_str() else {
        bail!("URL has no host");
    };
    // The WHATWG parser has already canonicalized every IPv4 spelling
    // (`2130706433`, `0x7f.1`, `0177.0.0.1`) to dotted-quad and bracketed
    // IPv6, so parsing `host_str` sees the address actually dialed.
    let literal = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = literal.parse::<IpAddr>() {
        return check_ip(ip, guard);
    }
    if is_local_name(host) {
        bail!("refusing to fetch `{host}`: local host names are blocked");
    }
    Ok(())
}

fn check_ip(ip: IpAddr, guard: &Guard) -> Result<()> {
    if guard.permits(ip) {
        Ok(())
    } else {
        bail!("refusing to fetch {ip}: private, loopback, link-local and other non-public addresses are blocked")
    }
}

/// `localhost` and its subdomains (RFC 6761 reserves the whole tree for
/// loopback), with or without the trailing root dot. Everything else that
/// resolves to a local address is caught by the resolved-IP check instead —
/// this is only so the common spelling fails with a clear message before any
/// DNS traffic.
fn is_local_name(name: &str) -> bool {
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    name == "localhost"
        || name.ends_with(".localhost")
        || name == "localhost.localdomain"
        || name == "ip6-localhost"
        || name == "ip6-loopback"
}

/// Every address `web_fetch` refuses to connect to — anything that is not
/// plainly public unicast. IPv6 addresses that embed an IPv4 one
/// (IPv4-mapped/-translated/-compatible, NAT64 incl. local-use, 6to4) are
/// judged by that embedded address, so no alternate spelling reaches a
/// blocked IPv4 range.
pub fn is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => is_blocked_v6(v6),
    }
}

fn is_blocked_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local() // 169.254/16, incl. the 169.254.169.254 cloud metadata endpoint
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_documentation()
        || a == 0 // 0.0.0.0/8 "this network"
        || (a == 100 && (64..128).contains(&b)) // CGNAT 100.64/10
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments 192.0.0/24
        || (a == 198 && (b == 18 || b == 19)) // benchmarking 198.18/15
        || a >= 240 // reserved 240/4
}

fn is_blocked_v6(ip: Ipv6Addr) -> bool {
    let embedded = embedded_v4(ip);
    if !embedded.is_empty() {
        return embedded.into_iter().any(is_blocked_v4);
    }
    let seg = ip.segments();
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || (seg[0] & 0xfe00) == 0xfc00 // unique local fc00::/7
        || (seg[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
        || (seg[0] & 0xffc0) == 0xfec0 // deprecated site-local fec0::/10
        || (seg[0] == 0x2001 && seg[1] == 0x0db8) // documentation 2001:db8::/32
        || (seg[0] == 0x2001 && seg[1] == 0) // Teredo 2001::/32 (tunnels to an obfuscated v4)
}

/// The IPv4 address(es) an IPv6 address carries, for the forms that route
/// to one: IPv4-mapped `::ffff:a.b.c.d`, IPv4-translated `::ffff:0:a.b.c.d`,
/// IPv4-compatible `::a.b.c.d` (excluding `::`/`::1`, their own v6 cases),
/// NAT64 `64:ff9b::/96`, NAT64 local-use `64:ff9b:1::/48` and 6to4
/// `2002::/16`. Local-use NAT64 prefixes may be any RFC 6052 length, so both
/// plausible embeddings (the `/48` split around the `u` octet, and the `/96`
/// tail) are returned — the caller blocks if *either* is blocked. Empty for
/// a native IPv6 address.
fn embedded_v4(ip: Ipv6Addr) -> Vec<Ipv4Addr> {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return vec![v4];
    }
    let seg = ip.segments();
    let tail = Ipv4Addr::from((u32::from(seg[6]) << 16) | u32::from(seg[7]));
    if seg[..6] == [0, 0, 0, 0, 0xffff, 0] {
        return vec![tail];
    }
    if seg[..6] == [0; 6] && !ip.is_unspecified() && !ip.is_loopback() {
        return vec![tail];
    }
    if seg[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return vec![tail];
    }
    if seg[..3] == [0x64, 0xff9b, 1] {
        let [a, b] = seg[3].to_be_bytes();
        let c = seg[4].to_be_bytes()[1];
        let d = seg[5].to_be_bytes()[0];
        return vec![Ipv4Addr::new(a, b, c, d), tail];
    }
    if seg[0] == 0x2002 {
        let [a, b] = seg[1].to_be_bytes();
        let [c, d] = seg[2].to_be_bytes();
        return vec![Ipv4Addr::new(a, b, c, d)];
    }
    Vec::new()
}

/// The fetch client's DNS resolver: resolve, then refuse the whole answer if
/// **any** address is blocked. Refusing (rather than filtering down to the
/// public addresses) keeps the outcome independent of which address the
/// connector happens to try first, and a name that mixes public and private
/// answers is suspicious enough to stop on.
pub(super) struct GuardedResolver {
    pub(super) guard: Guard,
}

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        let guard = self.guard.clone();
        Box::pin(async move {
            let addrs = lookup(&host, &guard).await?;
            if addrs.is_empty() {
                return Err(format!("`{host}` resolved to no addresses").into());
            }
            if let Some(bad) = addrs.iter().find(|a| !guard.permits(a.ip())) {
                return Err(format!(
                    "refusing to fetch `{host}`: it resolves to blocked address {}",
                    bad.ip()
                )
                .into());
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

async fn lookup(host: &str, guard: &Guard) -> std::io::Result<Vec<SocketAddr>> {
    #[cfg(test)]
    if let Some((_, ip)) = guard.fake_dns.iter().find(|(name, _)| name == host) {
        return Ok(vec![SocketAddr::new(*ip, 0)]);
    }
    #[cfg(not(test))]
    let _ = guard;
    Ok(tokio::net::lookup_host((host, 0)).await?.collect())
}
