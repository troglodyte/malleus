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
pub fn find_lease(contents: &str, mac: &str, name: Option<&str>) -> Option<Ipv4Addr> {
    let wanted_mac = normalize_mac(mac);
    eprintln!("debug: finding lease for MAC: {:?} (wanted: {:?}), name: {:?}", mac, wanted_mac, name);

    let mut ip: Option<Ipv4Addr> = None;
    let mut hw_matched = false;
    let mut name_matched = false;

    let normalized = contents.replace('{', " { ").replace('}', " } ");

    for word in normalized.split_ascii_whitespace() {
        if word == "{" {
            ip = None;
            hw_matched = false;
            name_matched = false;
        } else if word == "}" {
            if (hw_matched || name_matched) && let Some(ip_val) = ip {
                eprintln!("debug: matched record! ip={}, hw_matched={}, name_matched={}", ip_val, hw_matched, name_matched);
                return Some(ip_val);
            }
            ip = None;
            hw_matched = false;
            name_matched = false;
        } else if let Some(pair) = word.split_once('=') {
            let (key, value) = pair;
            let value = value.trim_end_matches(|c: char| c == ';' || c == ',');

            match key {
                "ip_address" => {
                    ip = value.parse().ok();
                }
                "hw_address" | "identifier" => {
                    if let Some(wanted) = &wanted_mac {
                        let raw = value.split_once(',').map_or(value, |(_, mac)| mac);
                        if let Some(norm) = normalize_mac(raw) {
                            if norm == *wanted {
                                hw_matched = true;
                            }
                        }
                    }
                }
                "name" => {
                    if let Some(wanted_name) = name {
                        if value == wanted_name {
                            name_matched = true;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    eprintln!("debug: no match found in lease file");
    None
}

/// Parse the output of `arp -an` and return the IP for `mac`.
pub fn find_in_arp(arp_output: &str, mac: &str) -> Option<Ipv4Addr> {
    let wanted_mac = normalize_mac(mac)?;
    eprintln!("debug: searching for MAC {} in ARP output", wanted_mac);

    for line in arp_output.lines() {
        // Example: ? (192.168.64.5) at 52:54:0:12:34:56 on bridge100 ifscope [ethernet]
        let parts: Vec<&str> = line.split_ascii_whitespace().collect();
        if parts.len() >= 4 && parts[2] == "at" {
            let ip_part = parts[1].trim_matches(|c| c == '(' || c == ')');
            let mac_part = parts[3];

            if let Some(norm_mac) = normalize_mac(mac_part) {
                if norm_mac == wanted_mac {
                    if let Ok(ip) = ip_part.parse::<Ipv4Addr>() {
                        eprintln!("debug: found MAC {} in ARP table: {}", wanted_mac, ip);
                        return Some(ip);
                    }
                }
            }
        }
    }

    eprintln!("debug: MAC {} not found in ARP table", wanted_mac);
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
        let found = find_lease(SAMPLE, "52:54:00:12:34:56", None);

        assert_eq!(found, Some(Ipv4Addr::new(192, 168, 64, 5)));
    }

    #[test]
    fn returns_none_when_mac_has_no_lease() {
        let found = find_lease(SAMPLE, "52:54:00:12:34:99", None);

        assert_eq!(found, None);
    }

    #[test]
    fn returns_none_for_malformed_target_mac() {
        let found = find_lease(SAMPLE, "not-a-mac", None);

        assert_eq!(found, None);
    }

    #[test]
    fn ignores_malformed_lease_records_and_keeps_scanning() {
        let contents = "\
{
\tip_address=not-an-ip
\thw_address=1,52:54:0:12:34:56
}
{
\tip_address=192.168.64.9
\thw_address=1,52:54:0:12:34:56
}
";

        let found = find_lease(contents, "52:54:00:12:34:56", None);

        assert_eq!(found, Some(Ipv4Addr::new(192, 168, 64, 9)));
    }

    #[test]
    fn finds_ip_in_arp_output() {
        let output = "\
? (10.200.210.1) at 30:8b:b2:ba:d7:f1 on en7 ifscope [ethernet]
? (192.168.64.5) at 52:54:0:12:34:56 on bridge100 ifscope [ethernet]
";
        let found = find_in_arp(output, "52:54:00:12:34:56");
        assert_eq!(found, Some(Ipv4Addr::new(192, 168, 64, 5)));
    }
}
