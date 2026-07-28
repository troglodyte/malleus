use std::fmt::Write;

/// A host share exposed to the guest with a virtio-fs mount tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountSpec {
    pub tag: String,
}

/// Inputs needed to render deterministic cloud-init payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionSpec {
    pub hostname: String,
    /// Address assigned to `incusbr0` in CIDR notation, e.g. `10.174.0.1/24`.
    pub bridge_cidr: String,
    pub client_cert_pem: String,
    pub server_cert_pem: String,
    pub server_key_pem: String,
    pub mounts: Vec<MountSpec>,
    /// Optional tag for the state directory share, used for IP discovery.
    pub state_tag: Option<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProvisionError {
    #[error("hostname must not be empty")]
    EmptyHostname,
    #[error("bridge_cidr must not be empty")]
    EmptyBridgeCidr,
    #[error("mount tag must not be empty")]
    EmptyMountTag,
    #[error("mount tag contains invalid characters: {0}")]
    InvalidMountTag(String),
}

fn validate_mount_tag(tag: &str) -> Result<(), ProvisionError> {
    if tag.trim().is_empty() {
        return Err(ProvisionError::EmptyMountTag);
    }
    if tag
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.')
    {
        Ok(())
    } else {
        Err(ProvisionError::InvalidMountTag(tag.to_string()))
    }
}

fn validate(spec: &ProvisionSpec) -> Result<(), ProvisionError> {
    if spec.hostname.trim().is_empty() {
        return Err(ProvisionError::EmptyHostname);
    }
    if spec.bridge_cidr.trim().is_empty() {
        return Err(ProvisionError::EmptyBridgeCidr);
    }
    for mount in &spec.mounts {
        validate_mount_tag(&mount.tag)?;
    }
    if let Some(tag) = &spec.state_tag {
        validate_mount_tag(tag)?;
    }
    Ok(())
}

fn push_block(out: &mut String, indent: usize, content: &str) {
    let prefix = " ".repeat(indent);
    for line in content.trim_end_matches('\n').lines() {
        out.push_str(&prefix);
        out.push_str(line);
        out.push('\n');
    }
}

fn render_preseed(bridge_cidr: &str) -> String {
    format!(
        "config:\n  core.https_address: \"[::]:8443\"\nnetworks:\n  - name: incusbr0\n    type: bridge\n    config:\n      ipv4.address: {bridge_cidr}\n      ipv4.nat: \"true\"\n      ipv6.address: none\nstorage_pools:\n  - name: default\n    driver: btrfs\n    config:\n      source: /dev/vdb\nprofiles:\n  - name: default\n    devices:\n      eth0:\n        type: nic\n        network: incusbr0\n        name: eth0\n"
    )
}


/// Render cloud-init `user-data` to install Incus and bootstrap host integration.
pub fn render_user_data(spec: &ProvisionSpec) -> Result<String, ProvisionError> {
    validate(spec)?;

    let preseed = render_preseed(&spec.bridge_cidr);

    let mut out = String::new();
    out.push_str("#cloud-config\n");
    out.push_str("output: {all: '| tee /dev/hvc0'}\n");

    if let Some(nc) = render_network_config() {
        out.push_str("network:\n");
        push_block(&mut out, 2, &nc);
    }

    out.push_str("bootcmd:\n");
    out.push_str("  - [sh, -c, \"echo $(date) bootcmd started > /dev/hvc0\"]\n");
    out.push_str("  - [ modprobe, virtio_console ]\n");
    out.push_str("  - [ modprobe, virtiofs ]\n");
    out.push_str("write_files:\n");
    out.push_str("  - path: /var/lib/incus/server.crt\n");
    out.push_str("    permissions: \"0644\"\n");
    out.push_str("    content: |\n");
    push_block(&mut out, 6, &spec.server_cert_pem);

    out.push_str("  - path: /var/lib/incus/server.key\n");
    out.push_str("    permissions: \"0600\"\n");
    out.push_str("    content: |\n");
    push_block(&mut out, 6, &spec.server_key_pem);

    out.push_str("  - path: /var/lib/malleus/client.crt\n");
    out.push_str("    permissions: \"0644\"\n");
    out.push_str("    content: |\n");
    push_block(&mut out, 6, &spec.client_cert_pem);

    out.push_str("  - path: /var/lib/malleus/preseed.yaml\n");
    out.push_str("    permissions: \"0644\"\n");
    out.push_str("    content: |\n");
    push_block(&mut out, 6, &preseed);

    if !spec.mounts.is_empty() || spec.state_tag.is_some() {
        out.push_str("mounts:\n");
        for mount in &spec.mounts {
            let _ = writeln!(
                out,
                "  - [ \"{}\", \"/mnt/mac/{}\", \"virtiofs\", \"defaults,_netdev\", \"0\", \"0\" ]",
                mount.tag, mount.tag
            );
        }
        if let Some(tag) = &spec.state_tag {
            let _ = writeln!(
                out,
                "  - [ \"{}\", \"/mnt/mac/{}\", \"virtiofs\", \"defaults,_netdev\", \"0\", \"0\" ]",
                tag, tag
            );
        }
    }

    out.push_str("runcmd:\n");
    out.push_str("  - [sh, -c, \"echo $(date) [malleus] runcmd started > /dev/hvc0\"]\n");
    out.push_str("  - [sh, -c, \"dmesg | grep -iE 'virtio|eth0|enp' > /dev/hvc0\"]\n");
    if let Some(tag) = &spec.state_tag {
        out.push_str(
            "  - [sh, -c, \"(while true; do TAG=\\\"$1\\\"; DIR=\\\"/mnt/mac/$TAG\\\"; echo \\\"$(date) [malleus] checking network and mounting $TAG\\\" > /dev/hvc0; mkdir -p \\\"$DIR\\\"; if ! mountpoint -q \\\"$DIR\\\"; then mount -t virtiofs \\\"$TAG\\\" \\\"$DIR\\\" || echo \\\"$(date) [malleus] virtio-fs mount failed\\\" > /dev/hvc0; fi; if [ -d \\\"$DIR\\\" ]; then IP=$(ip -4 addr show | grep 'inet ' | grep -v '127.0.0.1' | head -n 1 | awk '{print $2}' | cut -d/ -f1); if [ -n \\\"$IP\\\" ]; then echo \\\"$IP\\\" > \\\"$DIR\\\"/guest-ip; echo \\\"Reported IP $IP via virtio-fs\\\" | tee -a \\\"$DIR\\\"/guest-ip.log > /dev/hvc0; break; fi; fi; sleep 2; done) &\", \"\", \"",
        );
        out.push_str(tag);
        out.push_str("\"]\n");
    }

    out.push_str(
        "  - [sh, -c, \"(while true; do IP=$(ip -4 addr show | grep 'inet ' | grep -v '127.0.0.1' | head -n 1 | awk '{print $2}' | cut -d/ -f1); if [ -n \\\"$IP\\\" ]; then echo \\\"[malleus] Network is UP with IP $IP\\\" > /dev/hvc0; ping -c 1 192.168.64.1 > /dev/hvc0 2>&1 || echo \\\"[malleus] Host ping failed\\\" > /dev/hvc0; if command -v socat >/dev/null 2>&1; then echo \\\"Reported IP $IP via vsock\\\" > /dev/hvc0; echo \\\"$IP\\\" | socat - VSOCK-LISTEN:5,reuseaddr; break; fi; fi; sleep 2; done) &\", \"\", \"\"]\n",
    );

    out.push_str("  - [mkdir, -p, /etc/apt/keyrings]\n");
    out.push_str(
        "  - [sh, -c, \"curl -fsSL https://pkgs.zabbly.com/key.asc -o /etc/apt/keyrings/zabbly.asc\"]\n",
    );
    out.push_str(
        "  - [sh, -c, \"echo 'deb [signed-by=/etc/apt/keyrings/zabbly.asc] https://pkgs.zabbly.com/incus/stable /' > /etc/apt/sources.list.d/zabbly-incus-stable.list\"]\n",
    );
    out.push_str("  - [apt-get, update]\n");
    out.push_str("  - [apt-get, install, -y, incus, socat]\n");
    out.push_str("  - [sh, -c, \"incus admin init --preseed < /var/lib/malleus/preseed.yaml\"]\n");
    out.push_str(
        "  - [incus, config, trust, add-certificate, /var/lib/malleus/client.crt, --name, malleus-client, --type, client]\n",
    );

    for mount in &spec.mounts {
        let _ = writeln!(out, "  - [mkdir, -p, /mnt/mac/{}]", mount.tag);
    }
    if !spec.mounts.is_empty() {
        out.push_str("  - [mount, -a]\n");
    }

    Ok(out)
}

/// Render cloud-init `meta-data`.
pub fn render_meta_data(spec: &ProvisionSpec) -> Result<String, ProvisionError> {
    validate(spec)?;
    Ok(format!(
        "instance-id: malleus-{}\nlocal-hostname: {}\n",
        spec.hostname, spec.hostname
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ProvisionSpec {
        ProvisionSpec {
            hostname: "malleus".to_string(),
            bridge_cidr: "10.174.0.1/24".to_string(),
            client_cert_pem: "-----BEGIN CERTIFICATE-----\nCLIENT\n-----END CERTIFICATE-----\n"
                .to_string(),
            server_cert_pem: "-----BEGIN CERTIFICATE-----\nSERVER\n-----END CERTIFICATE-----\n"
                .to_string(),
            server_key_pem:
                "-----BEGIN PRIVATE KEY-----\nKEY\n-----END PRIVATE KEY-----\n".to_string(),
            mounts: Vec::new(),
            state_tag: None,
        }
    }

    #[test]
    fn user_data_includes_zabbly_repo_and_incus_preseed_bootstrap() {
        let user_data = render_user_data(&spec()).expect("user-data should render");

        assert!(
            user_data.contains("https://pkgs.zabbly.com/incus/stable"),
            "zabbly repo should be configured: {user_data}"
        );
        assert!(
            user_data.contains("incus admin init --preseed < /var/lib/malleus/preseed.yaml"),
            "preseed init command should be present: {user_data}"
        );
        assert!(
            user_data.contains("ipv4.address: 10.174.0.1/24"),
            "preseed should pin incusbr0 address: {user_data}"
        );
        assert!(
            user_data.contains("source: /dev/vdb"),
            "preseed should configure the dedicated btrfs disk: {user_data}"
        );
    }

    #[test]
    fn user_data_contains_client_and_server_certificate_material() {
        let user_data = render_user_data(&spec()).expect("user-data should render");

        assert!(user_data.contains("/var/lib/incus/server.crt"));
        assert!(user_data.contains("/var/lib/incus/server.key"));
        assert!(user_data.contains("/var/lib/malleus/client.crt"));
        assert!(user_data.contains("SERVER"));
        assert!(user_data.contains("KEY"));
        assert!(user_data.contains("CLIENT"));
    }

    #[test]
    fn metadata_uses_hostname_for_instance_identity() {
        let meta = render_meta_data(&spec()).expect("meta-data should render");

        assert_eq!(
            meta,
            "instance-id: malleus-malleus\nlocal-hostname: malleus\n"
        );
    }

    #[test]
    fn mounts_render_cloud_init_mounts_module() {
        let mut spec = spec();
        spec.mounts = vec![
            MountSpec {
                tag: "code".to_string(),
            },
            MountSpec {
                tag: "data".to_string(),
            },
        ];

        let user_data = render_user_data(&spec).expect("user-data should render");

        assert!(user_data.contains("mounts:"));
        assert!(user_data.contains("[ \"code\", \"/mnt/mac/code\", \"virtiofs\", \"defaults,_netdev\", \"0\", \"0\" ]"));
        assert!(user_data.contains("[ \"data\", \"/mnt/mac/data\", \"virtiofs\", \"defaults,_netdev\", \"0\", \"0\" ]"));
    }

    #[test]
    fn invalid_mount_tag_is_rejected() {
        let mut spec = spec();
        spec.mounts = vec![MountSpec {
            tag: "bad/tag".to_string(),
        }];

        let err = render_user_data(&spec).expect_err("invalid mount tag should fail");

        assert_eq!(err, ProvisionError::InvalidMountTag("bad/tag".to_string()));
    }

    #[test]
    fn state_tag_renders_mount_and_ip_reporting_script() {
        let mut spec = spec();
        spec.state_tag = Some("malleus-state".to_string());

        let user_data = render_user_data(&spec).expect("user-data should render");
        let network_config = render_network_config();

        assert!(user_data.contains("[ \"malleus-state\", \"/mnt/mac/malleus-state\", \"virtiofs\", \"defaults,_netdev\", \"0\", \"0\" ]"));
        assert!(user_data.contains("ip -4 addr show"));
        assert!(user_data.contains("> \\\"$DIR\\\"/guest-ip"));
        assert!(network_config.is_some());
        assert!(network_config.unwrap().contains("dhcp4: true"));
    }
}
/// Render cloud-init `network-config` to enable DHCP on all interfaces.
pub fn render_network_config() -> Option<String> {
    let mut out = String::new();
    out.push_str("version: 2\n");
    out.push_str("ethernets:\n");
    out.push_str("  all:\n");
    out.push_str("    match:\n");
    out.push_str("      name: \"*\"\n");
    out.push_str("    dhcp4: true\n");
    Some(out)
}
