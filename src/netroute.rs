pub use ipnet::Ipv4Net;

/// Return the first existing route that overlaps `wanted`, if any.
///
/// Two CIDR blocks are either disjoint or nested, so an overlap means one
/// contains the other. A conflict is fatal for the routable-IP design: traffic
/// to a container would follow the pre-existing route instead of ours, so the
/// caller should refuse to start and prompt for a different bridge subnet.
pub fn find_conflict(wanted: Ipv4Net, existing: &[Ipv4Net]) -> Option<Ipv4Net> {
    existing
        .iter()
        .copied()
        .find(|route| route.contains(&wanted) || wanted.contains(route))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(cidr: &str) -> Ipv4Net {
        cidr.parse().expect("test CIDR should be valid")
    }

    #[test]
    fn detects_a_wider_existing_route_that_would_swallow_our_subnet() {
        // A corporate VPN pushing 10/8 is the realistic case: our bridge subnet
        // sits inside it, so the host route would never be preferred.
        let existing = [net("10.0.0.0/8"), net("192.168.64.0/24")];

        let conflict = find_conflict(net("10.174.0.0/24"), &existing);

        assert_eq!(conflict, Some(net("10.0.0.0/8")));
    }

    #[test]
    fn reports_no_conflict_when_nothing_overlaps() {
        let existing = [net("172.16.0.0/12"), net("192.168.64.0/24")];

        let conflict = find_conflict(net("10.174.0.0/24"), &existing);

        assert_eq!(conflict, None);
    }
}
