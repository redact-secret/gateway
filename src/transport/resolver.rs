//! Destination address policy and the pinned resolver (SSRF and rebinding defense).
//!
//! The HTTP client resolves hostnames only through [`PolicyResolver`]. It resolves, then
//! validates every returned address against [`is_public_destination`], and hands the
//! client exactly those validated addresses. The client connects only to addresses it was
//! handed, so validation and connection use the same resolution: there is no second lookup
//! between check and use. A single disallowed address in an answer rejects the whole
//! answer (an attacker-controlled name cannot hide a private address among public ones),
//! and an empty answer is rejected.
//!
//! Residual (documented, not solved here): the system resolver and the network are trusted
//! to return the real provider addresses. DNS poisoning that yields a different *public*
//! address is stopped by TLS hostname verification, not by this module.

use std::fmt;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// Whether an address may be used as an upstream destination for the supported
/// public-provider deployment.
///
/// IPv4 rejected: unspecified/"this network" (0/8), private (10/8, 172.16/12, 192.168/16),
/// shared address space (100.64/10), loopback (127/8), link-local incl. cloud metadata
/// (169.254/16), IETF protocol and documentation ranges (192.0.0/24, 192.0.2/24,
/// 198.51.100/24, 203.0.113/24), 6to4 relay (192.88.99/24), benchmarking (198.18/15),
/// multicast (224/4), and reserved/broadcast (240/4).
///
/// IPv6 is allowlisted: only global unicast `2000::/3` is accepted, minus IETF protocol
/// assignments `2001::/23` (Teredo, ORCHID), documentation `2001:db8::/32` and
/// `3fff::/20`, and 6to4 `2002::/16`. Everything else is rejected, which covers
/// unspecified, loopback, IPv4-mapped (`::ffff:0:0/96`), IPv4-compatible, NAT64
/// (`64:ff9b::/96`), unique-local (`fc00::/7`), link-local (`fe80::/10`), site-local, and
/// multicast, so an IPv4 address smuggled inside an IPv6 one cannot pass.
#[must_use]
pub const fn is_public_destination(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

const fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0
        || a == 10
        || a == 127
        || a >= 224
        || (a == 100 && (b & 0xC0) == 64)
        || (a == 169 && b == 254)
        || (a == 172 && (b & 0xF0) == 16)
        || (a == 192 && b == 0 && (c == 0 || c == 2))
        || (a == 192 && b == 88 && c == 99)
        || (a == 192 && b == 168)
        || (a == 198 && (b & 0xFE) == 18)
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113))
}

const fn is_public_v6(ip: Ipv6Addr) -> bool {
    let [s0, s1, ..] = ip.segments();
    if (s0 & 0xE000) != 0x2000 {
        return false;
    }
    !((s0 == 0x2001 && s1 < 0x0200)
        || (s0 == 0x2001 && s1 == 0x0db8)
        || s0 == 0x2002
        || (s0 == 0x3fff && (s1 & 0xF000) == 0))
}

/// Which addresses a client may connect to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AddressPolicy {
    /// Public-provider deployment: [`is_public_destination`] only.
    Public,
    /// Unit tests only: additionally permit loopback so a local fake can stand in.
    #[cfg(test)]
    PublicOrLoopback,
}

impl AddressPolicy {
    const fn permits(self, ip: IpAddr) -> bool {
        match self {
            Self::Public => is_public_destination(ip),
            #[cfg(test)]
            Self::PublicOrLoopback => is_public_destination(ip) || ip.is_loopback(),
        }
    }
}

type LookupFuture = Pin<Box<dyn Future<Output = Option<Vec<IpAddr>>> + Send>>;

/// Source of raw (unvalidated) addresses for a hostname. The policy is applied to whatever
/// a source returns, so a source can never widen what is permitted.
pub(crate) trait AddressSource: Send + Sync {
    fn lookup(&self, host: &str) -> LookupFuture;
}

/// The operating-system resolver (`getaddrinfo` via tokio). Honors no proxy or override
/// from the gateway's environment beyond what the host's resolver itself does.
#[derive(Debug)]
pub(crate) struct SystemSource;

impl AddressSource for SystemSource {
    fn lookup(&self, host: &str) -> LookupFuture {
        let host = host.to_owned();
        Box::pin(async move {
            let found = tokio::net::lookup_host((host.as_str(), 0)).await.ok()?;
            Some(found.map(|a| a.ip()).collect())
        })
    }
}

/// Resolver failure. A fixed code; never contains the name or any address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResolveError {
    /// The name is not an allowed upstream host.
    HostNotAllowed,
    /// Lookup failed or returned nothing.
    NoAddresses,
    /// At least one returned address is outside the destination policy.
    AddressDenied,
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::HostNotAllowed => "destination_host_not_allowed",
            Self::NoAddresses => "destination_unresolved",
            Self::AddressDenied => "destination_address_denied",
        })
    }
}

impl std::error::Error for ResolveError {}

/// The client's only resolver: allowed names only, validated addresses only.
#[derive(Clone)]
pub(crate) struct PolicyResolver {
    allowed: Arc<[Box<str>]>,
    source: Arc<dyn AddressSource>,
    policy: AddressPolicy,
}

impl fmt::Debug for PolicyResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PolicyResolver")
            .field("allowed_hosts", &self.allowed.len())
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl PolicyResolver {
    pub(crate) fn new(
        allowed: Vec<Box<str>>,
        source: Arc<dyn AddressSource>,
        policy: AddressPolicy,
    ) -> Self {
        Self {
            allowed: allowed.into(),
            source,
            policy,
        }
    }

    /// Production resolver: system lookup, public addresses only.
    pub(crate) fn system(allowed: Vec<Box<str>>) -> Self {
        Self::new(allowed, Arc::new(SystemSource), AddressPolicy::Public)
    }

    /// Resolve and validate. Port is left at 0: the client substitutes the URL's port.
    pub(crate) async fn resolve_validated(
        &self,
        host: &str,
    ) -> Result<Vec<SocketAddr>, ResolveError> {
        if !self.allowed.iter().any(|h| &**h == host) {
            return Err(ResolveError::HostNotAllowed);
        }
        let ips = self
            .source
            .lookup(host)
            .await
            .filter(|ips| !ips.is_empty())
            .ok_or(ResolveError::NoAddresses)?;
        if ips.iter().any(|ip| !self.policy.permits(*ip)) {
            return Err(ResolveError::AddressDenied);
        }
        Ok(ips.into_iter().map(|ip| SocketAddr::new(ip, 0)).collect())
    }
}

impl Resolve for PolicyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let this = self.clone();
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs = this.resolve_validated(&host).await?;
            let iter: Addrs = Box::new(addrs.into_iter());
            Ok(iter)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("ip literal")
    }

    #[test]
    fn disallowed_addresses_are_rejected() {
        for s in [
            "0.0.0.0",
            "0.1.2.3",
            "10.0.0.1",
            "100.64.0.1",
            "100.127.255.255",
            "100.100.100.200",
            "127.0.0.1",
            "127.255.255.254",
            "169.254.169.254",
            "169.254.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.0.0.1",
            "192.0.2.1",
            "192.88.99.1",
            "192.168.1.1",
            "198.18.0.1",
            "198.19.255.255",
            "198.51.100.7",
            "203.0.113.9",
            "224.0.0.1",
            "239.255.255.255",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "::ffff:8.8.8.8",
            "::127.0.0.1",
            "64:ff9b::7f00:1",
            "64:ff9b::808:808",
            "fc00::1",
            "fd00:ec2::254",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "2001::1",
            "2001:0:4136:e378:8000:63bf:3fff:fdd2",
            "2001:db8::1",
            "2002:7f00:1::",
            "3fff::1",
        ] {
            assert!(!is_public_destination(ip(s)), "must reject {s}");
        }
    }

    #[test]
    fn public_addresses_are_accepted() {
        for s in [
            "8.8.8.8",
            "1.1.1.1",
            "172.15.255.255",
            "172.32.0.1",
            "100.63.255.255",
            "100.128.0.1",
            "198.17.0.1",
            "198.20.0.1",
            "192.0.1.1",
            "192.169.0.1",
            "223.255.255.255",
            "2606:4700::1111",
            "2001:4860:4860::8888",
            "2001:200::1",
        ] {
            assert!(is_public_destination(ip(s)), "must accept {s}");
        }
    }

    struct MapSource(HashMap<&'static str, Vec<IpAddr>>);

    impl AddressSource for MapSource {
        fn lookup(&self, host: &str) -> LookupFuture {
            let found = self.0.get(host).cloned();
            Box::pin(async move { found })
        }
    }

    fn resolver(answers: &[(&'static str, &[&str])]) -> PolicyResolver {
        let map = answers
            .iter()
            .map(|(h, ips)| (*h, ips.iter().map(|s| ip(s)).collect()))
            .collect();
        PolicyResolver::new(
            vec!["api.example.test".into()],
            Arc::new(MapSource(map)),
            AddressPolicy::Public,
        )
    }

    #[tokio::test]
    async fn answers_are_validated_whole_and_pinned() {
        let r = resolver(&[
            ("api.example.test", &["8.8.8.8", "2606:4700::1111"]),
            ("mixed.example.test", &["8.8.8.8", "127.0.0.1"]),
        ]);
        let ok = r
            .resolve_validated("api.example.test")
            .await
            .expect("public");
        assert_eq!(ok.len(), 2);
        assert!(ok.iter().all(|a| a.port() == 0));
        // A name outside the allowlist is never looked up, even with a public answer.
        assert_eq!(
            r.resolve_validated("mixed.example.test").await,
            Err(ResolveError::HostNotAllowed)
        );
    }

    #[tokio::test]
    async fn mixed_private_answer_and_empty_answer_reject() {
        let mut r = resolver(&[("api.example.test", &["8.8.8.8", "10.0.0.5"])]);
        assert_eq!(
            r.resolve_validated("api.example.test").await,
            Err(ResolveError::AddressDenied)
        );
        r = resolver(&[("api.example.test", &[])]);
        assert_eq!(
            r.resolve_validated("api.example.test").await,
            Err(ResolveError::NoAddresses)
        );
        r = resolver(&[]);
        assert_eq!(
            r.resolve_validated("api.example.test").await,
            Err(ResolveError::NoAddresses)
        );
        for bad in [
            "127.0.0.1",
            "169.254.169.254",
            "::1",
            "::ffff:10.0.0.1",
            "fd00::1",
        ] {
            r = resolver(&[("api.example.test", &[bad])]);
            assert_eq!(
                r.resolve_validated("api.example.test").await,
                Err(ResolveError::AddressDenied),
                "{bad}"
            );
        }
    }

    #[tokio::test]
    async fn rebinding_second_answer_is_checked_independently() {
        // Each resolution is validated on its own: a first public answer grants nothing to
        // a later private one.
        struct Flip(std::sync::atomic::AtomicBool);
        impl AddressSource for Flip {
            fn lookup(&self, _host: &str) -> LookupFuture {
                let first = self.0.swap(false, std::sync::atomic::Ordering::SeqCst);
                let answer = if first {
                    ip("8.8.8.8")
                } else {
                    ip("127.0.0.1")
                };
                Box::pin(async move { Some(vec![answer]) })
            }
        }
        let r = PolicyResolver::new(
            vec!["api.example.test".into()],
            Arc::new(Flip(std::sync::atomic::AtomicBool::new(true))),
            AddressPolicy::Public,
        );
        assert!(r.resolve_validated("api.example.test").await.is_ok());
        assert_eq!(
            r.resolve_validated("api.example.test").await,
            Err(ResolveError::AddressDenied)
        );
    }
}
