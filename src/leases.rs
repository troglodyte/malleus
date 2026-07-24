use std::net::Ipv4Addr;

/// Rewrite a MAC into a canonical lowercase, zero-padded form.
///
/// macOS writes lease MACs with leading zeros stripped (`52:54:0:12:34:56`),
/// while we configure vfkit with the padded form (`52:54:00:12:34:56`).
/// Both normalize to the same string, so comparison is exact rather than fuzzy.
fn normalize_mac(mac: &str) -> Option<String> {
    let mut octets = Vec::new();
    for part in mac.split(':') {
        let byte = u8::from_str_radix(part, 16).ok()?;
        octets.push(format!("{byte:02x}"));
    }
    if octets.len() != 6 {
        return None;
    }
    Some(octets.join(":"))
}

/// Find the IPv4 address leased to `mac` in the contents of `/var/db/dhcpd_leases`.
///
/// Returns `None` when the MAC is malformed or has no current lease.
pub fn find_lease(contents: &str, mac: &str) -> Option<Ipv4Addr> {
    let wanted = normalize_mac(mac)?;

    let mut ip: Option<Ipv4Addr> = None;
    let mut hw: Option<String> = None;

    for line in contents.lines() {
        let line = line.trim();

        if line == "{" {
            ip = None;
            hw = None;
        } else if line == "}" {
            if hw.as_deref() == Some(wanted.as_str())
                && let Some(ip) = ip
            {
                return Some(ip);
            }
            ip = None;
            hw = None;
        } else if let Some(value) = line.strip_prefix("ip_address=") {
            ip = value.parse().ok();
        } else if let Some(value) = line.strip_prefix("hw_address=") {
            // Values carry a hardware-type prefix, e.g. `1,52:54:0:12:34:56`.
            let raw = value.split_once(',').map_or(value, |(_, mac)| mac);
            hw = normalize_mac(raw);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    /// Representative `/var/db/dhcpd_leases`. Note that macOS strips leading
    /// zeros from MAC octets, so `52:54:00:...` is written `52:54:0:...`.
    const SAMPLE: &str = "\
{
\tname=other-vm
\tip_address=192.168.64.4
\thw_address=1,aa:bb:cc:dd:ee:ff
\tidentifier=1,aa:bb:cc:dd:ee:ff
\tlease=0x687a1000
}
{
\tname=debian
\tip_address=192.168.64.5
\thw_address=1,52:54:0:12:34:56
\tidentifier=1,52:54:0:12:34:56
\tlease=0x687a2000
}
";

    #[test]
    fn finds_ip_for_mac_written_with_stripped_leading_zeros() {
        let found = find_lease(SAMPLE, "52:54:00:12:34:56");

        assert_eq!(found, Some(Ipv4Addr::new(192, 168, 64, 5)));
    }
}
