//! SSRF address filter (design doc §8): the predicate deciding whether a
//! resolved IP address may be connected to.
//!
//! This is the single arbiter of address safety. It is hand-rolled with
//! octet/segment checks rather than the (mostly unstable) `std::net` range
//! predicates so the exact set of blocked ranges is explicit and testable,
//! and so IPv4-mapped/compatible IPv6 forms recurse into the IPv4 rules
//! (closing the `[::ffff:127.0.0.1]` style bypass). Numeric-literal host
//! forms (`2130706433`, `0x7f.1`, …) are normalized to real addresses by the
//! URL parser *and* by the OS resolver before they reach here, and every
//! resolved address is filtered — so those forms are closed by construction,
//! not by string matching.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Returns `true` if `ip` is a globally-routable unicast address safe to
/// connect to, `false` for any loopback, private, link-local, CGNAT,
/// metadata, documentation, multicast, or otherwise-reserved address.
#[must_use]
pub fn ip_allowed(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => ipv4_allowed(v4),
        IpAddr::V6(v6) => ipv6_allowed(v6),
    }
}

/// IPv4 rules. Blocks every non-global-unicast range.
fn ipv4_allowed(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    let (a, b) = (o[0], o[1]);
    // 0.0.0.0/8 — "this network" (includes the unspecified address).
    if a == 0 {
        return false;
    }
    // 10.0.0.0/8 — RFC 1918 private.
    if a == 10 {
        return false;
    }
    // 100.64.0.0/10 — CGNAT (RFC 6598).
    if a == 100 && (64..=127).contains(&b) {
        return false;
    }
    // 127.0.0.0/8 — loopback.
    if a == 127 {
        return false;
    }
    // 169.254.0.0/16 — link-local, incl. cloud metadata 169.254.169.254.
    if a == 169 && b == 254 {
        return false;
    }
    // 172.16.0.0/12 — RFC 1918 private.
    if a == 172 && (16..=31).contains(&b) {
        return false;
    }
    // 192.0.0.0/24 — IETF protocol assignments.
    if o[0..3] == [192, 0, 0] {
        return false;
    }
    // 192.0.2.0/24 — documentation (TEST-NET-1).
    if o[0..3] == [192, 0, 2] {
        return false;
    }
    // 192.88.99.0/24 — deprecated 6to4 relay anycast. IANA marks the whole
    // block (and 192.88.99.2, the 6a44 relay) Globally Reachable = False.
    if o[0..3] == [192, 88, 99] {
        return false;
    }
    // 192.168.0.0/16 — RFC 1918 private.
    if a == 192 && b == 168 {
        return false;
    }
    // 198.18.0.0/15 — benchmarking.
    if a == 198 && (b == 18 || b == 19) {
        return false;
    }
    // 198.51.100.0/24 — documentation (TEST-NET-2).
    if o[0..3] == [198, 51, 100] {
        return false;
    }
    // 203.0.113.0/24 — documentation (TEST-NET-3).
    if o[0..3] == [203, 0, 113] {
        return false;
    }
    // 224.0.0.0/4 multicast + 240.0.0.0/4 reserved + 255.255.255.255 broadcast.
    if a >= 224 {
        return false;
    }
    true
}

/// IPv6 rules. Recurses into the IPv4 rules for every embedded-IPv4 form.
fn ipv6_allowed(ip: Ipv6Addr) -> bool {
    // :: and ::1 first, so the embedded-v4 recursion below never sees them.
    if ip.is_unspecified() || ip.is_loopback() {
        return false;
    }
    // ::ffff:a.b.c.d — IPv4-mapped: apply the IPv4 rules to the embedded v4.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return ipv4_allowed(v4);
    }
    // ::a.b.c.d — deprecated IPv4-compatible: `to_ipv4` matches ::/96 (the
    // mapped case is already handled above).
    if let Some(v4) = ip.to_ipv4() {
        return ipv4_allowed(v4);
    }
    let segs = ip.segments();
    let hi = (segs[0] >> 8) as u8;
    // fc00::/7 — unique local (ULA).
    if hi & 0xfe == 0xfc {
        return false;
    }
    // fe80::/10 — link-local.
    if segs[0] & 0xffc0 == 0xfe80 {
        return false;
    }
    // fec0::/10 — deprecated site-local. RFC 3879 removed it from the registry
    // but explicitly tolerates existing deployments and tells routers to filter
    // it; an SSRF boundary cannot delegate that to somebody else's router.
    if segs[0] & 0xffc0 == 0xfec0 {
        return false;
    }
    // ff00::/8 — multicast.
    if hi == 0xff {
        return false;
    }
    // 100::/64 (discard-only, RFC 6666) and 100:0:0:1::/64 (dummy prefix,
    // RFC 9780) — one branch, since they differ only in the fourth hextet.
    if segs[0] == 0x0100 && segs[1] == 0 && segs[2] == 0 && segs[3] <= 1 {
        return false;
    }
    // 2001:db8::/32 — documentation. *Not* covered by the 2001::/23 branch
    // below (0x0db8 > 0x01ff), so it needs its own test.
    if segs[0] == 0x2001 && segs[1] == 0x0db8 {
        return false;
    }
    // 2001::/23 — "IETF Protocol Assignments", Globally Reachable = False in
    // the IANA registry. One branch covers Teredo (2001::/32, which tunnels an
    // obfuscated IPv4 endpoint we decline to de-obfuscate), benchmarking
    // (2001:2::/48) and deprecated ORCHID (2001:10::/28).
    //
    // The registry does carve four Globally Reachable = True exceptions out of
    // this block — 2001:1::1/128 (PCP), 2001:3::/32 (AMT), 2001:20::/28
    // (ORCHIDv2) and 2001:30::/28 (DET). Overblocking them is deliberate, the
    // same trade already made for 192.0.0.0/24: none is a fetchable web origin,
    // and a coarse boundary is the one that stays correct as the registry moves.
    if segs[0] == 0x2001 && segs[1] <= 0x01ff {
        return false;
    }
    // 3fff::/20 — documentation (RFC 9637).
    if segs[0] == 0x3fff && segs[1] >> 12 == 0 {
        return false;
    }
    // 5f00::/16 — SRv6 SIDs (RFC 9602), Globally Reachable = False.
    if segs[0] == 0x5f00 {
        return false;
    }
    // 2002::/16 — 6to4 embeds an IPv4 address in the next two hextets.
    if segs[0] == 0x2002 {
        let embedded = Ipv4Addr::new(
            (segs[1] >> 8) as u8,
            (segs[1] & 0xff) as u8,
            (segs[2] >> 8) as u8,
            (segs[2] & 0xff) as u8,
        );
        return ipv4_allowed(embedded);
    }
    // 64:ff9b::/96 — NAT64 well-known prefix embeds IPv4 in the last hextets.
    if segs[0] == 0x0064 && segs[1] == 0xff9b && segs[2..6] == [0, 0, 0, 0] {
        let embedded = Ipv4Addr::new(
            (segs[6] >> 8) as u8,
            (segs[6] & 0xff) as u8,
            (segs[7] >> 8) as u8,
            (segs[7] & 0xff) as u8,
        );
        return ipv4_allowed(embedded);
    }
    // 64:ff9b:1::/48 — NAT64 local-use prefixes. Unlike the well-known
    // 64:ff9b::/96 above (Globally Reachable = True, which is why it recurses
    // into the embedded IPv4 rather than being rejected outright), IANA marks
    // this one False: it names a *local* translator, and its low 80 bits carry
    // no fixed IPv4 layout to recurse into. The branches cannot both match —
    // the one above requires `segs[2..6] == [0, 0, 0, 0]`.
    if segs[0] == 0x0064 && segs[1] == 0xff9b && segs[2] == 0x0001 {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn v4(s: &str) -> IpAddr {
        IpAddr::V4(Ipv4Addr::from_str(s).unwrap())
    }
    fn v6(s: &str) -> IpAddr {
        IpAddr::V6(Ipv6Addr::from_str(s).unwrap())
    }

    #[test]
    fn allows_public_v4() {
        assert!(ip_allowed(v4("1.1.1.1")));
        assert!(ip_allowed(v4("8.8.8.8")));
        assert!(ip_allowed(v4("93.184.216.34"))); // example.com
        assert!(ip_allowed(v4("223.255.255.255")));
    }

    #[test]
    fn blocks_loopback_v4() {
        assert!(!ip_allowed(v4("127.0.0.1")));
        assert!(!ip_allowed(v4("127.1.2.3")));
        assert!(!ip_allowed(v4("127.255.255.255")));
    }

    #[test]
    fn blocks_rfc1918() {
        assert!(!ip_allowed(v4("10.0.0.1")));
        assert!(!ip_allowed(v4("10.255.255.255")));
        assert!(!ip_allowed(v4("172.16.0.1")));
        assert!(!ip_allowed(v4("172.31.255.255")));
        assert!(ip_allowed(v4("172.32.0.1"))); // just outside /12
        assert!(ip_allowed(v4("172.15.255.255"))); // just below /12
        assert!(!ip_allowed(v4("192.168.0.1")));
        assert!(!ip_allowed(v4("192.168.255.255")));
    }

    #[test]
    fn blocks_link_local_and_metadata() {
        assert!(!ip_allowed(v4("169.254.0.1")));
        assert!(!ip_allowed(v4("169.254.169.254"))); // cloud metadata
    }

    #[test]
    fn blocks_cgnat() {
        assert!(!ip_allowed(v4("100.64.0.1")));
        assert!(!ip_allowed(v4("100.127.255.255")));
        assert!(ip_allowed(v4("100.63.255.255"))); // below /10
        assert!(ip_allowed(v4("100.128.0.1"))); // above /10
    }

    #[test]
    fn blocks_unspecified_broadcast_multicast_reserved() {
        assert!(!ip_allowed(v4("0.0.0.0")));
        assert!(!ip_allowed(v4("255.255.255.255")));
        assert!(!ip_allowed(v4("224.0.0.1")));
        assert!(!ip_allowed(v4("239.255.255.255")));
        assert!(!ip_allowed(v4("240.0.0.1")));
    }

    #[test]
    fn blocks_documentation_and_benchmark() {
        assert!(!ip_allowed(v4("192.0.2.1")));
        assert!(!ip_allowed(v4("198.51.100.1")));
        assert!(!ip_allowed(v4("203.0.113.1")));
        assert!(!ip_allowed(v4("198.18.0.1")));
        assert!(!ip_allowed(v4("198.19.255.255")));
        assert!(!ip_allowed(v4("192.0.0.1")));
    }

    #[test]
    fn blocks_6to4_relay_anycast() {
        assert!(!ip_allowed(v4("192.88.99.1")));
        assert!(!ip_allowed(v4("192.88.99.2"))); // the 6a44 relay
        assert!(!ip_allowed(v4("192.88.99.255")));
        // Neighbours outside the /24 stay reachable.
        assert!(ip_allowed(v4("192.88.98.1")));
        assert!(ip_allowed(v4("192.88.100.1")));
    }

    #[test]
    fn blocks_loopback_and_local_v6() {
        assert!(!ip_allowed(v6("::1")));
        assert!(!ip_allowed(v6("::")));
        assert!(!ip_allowed(v6("fe80::1")));
        assert!(!ip_allowed(v6("fc00::1")));
        assert!(!ip_allowed(v6("fd12:3456::1")));
        assert!(!ip_allowed(v6("2001:db8::1")));
        assert!(!ip_allowed(v6("ff02::1")));
    }

    #[test]
    fn allows_public_v6() {
        assert!(ip_allowed(v6("2606:4700:4700::1111"))); // cloudflare
        assert!(ip_allowed(v6("2a00:1450:4001::200e"))); // google
    }

    #[test]
    fn v4_mapped_and_compat_recurse() {
        // ::ffff:127.0.0.1 must be treated as loopback.
        assert!(!ip_allowed(v6("::ffff:127.0.0.1")));
        assert!(!ip_allowed(v6("::ffff:7f00:1")));
        assert!(!ip_allowed(v6("::ffff:10.0.0.1")));
        assert!(!ip_allowed(v6("::ffff:169.254.169.254")));
        // A mapped *public* address is still fine.
        assert!(ip_allowed(v6("::ffff:8.8.8.8")));
    }

    #[test]
    fn embedded_v4_tunnels_recurse() {
        // 6to4 wrapping a loopback address.
        assert!(!ip_allowed(v6("2002:7f00:0001::"))); // 2002:127.0.0.1
        // 6to4 wrapping a public address is allowed.
        assert!(ip_allowed(v6("2002:0808:0808::"))); // 2002:8.8.8.8
        // NAT64 wrapping loopback.
        assert!(!ip_allowed(v6("64:ff9b::7f00:1")));
        // Teredo range blocked outright.
        assert!(!ip_allowed(v6("2001:0:0:0:0:0:0:1")));
    }

    #[test]
    fn nat64_well_known_recurses_but_local_use_is_blocked() {
        // 64:ff9b::/96 is Globally Reachable: the embedded IPv4 decides.
        assert!(ip_allowed(v6("64:ff9b::8.8.8.8")));
        assert!(!ip_allowed(v6("64:ff9b::10.0.0.1")));
        // 64:ff9b:1::/48 is not: blocked whatever it appears to embed.
        assert!(!ip_allowed(v6("64:ff9b:1::8.8.8.8")));
        assert!(!ip_allowed(v6("64:ff9b:1::1")));
        assert!(!ip_allowed(v6("64:ff9b:1:ffff:ffff:ffff:ffff:ffff")));
        // The neighbouring /48s are outside both.
        assert!(ip_allowed(v6("64:ff9b:2::1")));
    }

    #[test]
    fn blocks_ietf_protocol_assignments_v6() {
        // 2001::/23, whole range.
        assert!(!ip_allowed(v6("2001::1"))); // Teredo
        assert!(!ip_allowed(v6("2001:2::1"))); // benchmarking
        assert!(!ip_allowed(v6("2001:10::1"))); // deprecated ORCHID
        assert!(!ip_allowed(v6("2001:1ff:ffff:ffff:ffff:ffff:ffff:ffff"))); // last of /23
        // Documentation sits above the /23 and needs its own branch.
        assert!(!ip_allowed(v6("2001:db8::1")));
        // The first hextet-pair past the /23 is reachable again.
        assert!(ip_allowed(v6("2001:200::1"))); // WIDE, a real allocation
    }

    #[test]
    fn blocks_discard_dummy_documentation_and_srv6() {
        // 100::/64 discard-only and 100:0:0:1::/64 dummy prefix.
        assert!(!ip_allowed(v6("100::1")));
        assert!(!ip_allowed(v6("100:0:0:1::1")));
        assert!(ip_allowed(v6("100:0:0:2::1"))); // just past the pair
        // 3fff::/20 documentation.
        assert!(!ip_allowed(v6("3fff::1")));
        assert!(!ip_allowed(v6("3fff:fff:ffff:ffff:ffff:ffff:ffff:ffff")));
        assert!(ip_allowed(v6("3fff:1000::1"))); // 21st bit set: outside /20
        assert!(ip_allowed(v6("4000::1")));
        // 5f00::/16 SRv6 SIDs.
        assert!(!ip_allowed(v6("5f00::1")));
        assert!(ip_allowed(v6("5f01::1")));
        // fec0::/10 deprecated site-local.
        assert!(!ip_allowed(v6("fec0::1")));
        assert!(!ip_allowed(v6("feff:ffff::1")));
    }
}
