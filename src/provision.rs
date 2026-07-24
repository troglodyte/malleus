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

fn render_fstab_mounts(mounts: &[MountSpec]) -> String {
    let mut fstab = String::new();
    for mount in mounts {
        let _ = writeln!(
            fstab,
            "{} /mnt/mac/{} virtiofs defaults,_netdev 0 0",
            mount.tag, mount.tag
        );
    }
    fstab
}

/// Render cloud-init `user-data` to install Incus and bootstrap host integration.
pub fn render_user_data(spec: &ProvisionSpec) -> Result<String, ProvisionError> {
    validate(spec)?;

    let preseed = render_preseed(&spec.bridge_cidr);
    let fstab_mounts = render_fstab_mounts(&spec.mounts);

    let mut out = String::new();

    out.push_str("#cloud-config\n");
    out.push_str("write_files:\n");
    out.push_str("  - path: /var/lib/incus/server.crt\n");
    out.push_str("    permissions: \"0644\"\n");
    out.push_str("    content: |\n");
    push_block(&mut out, 6, &spec.server_cert_pem);

    out.push_str("  - path: /var/lib/incus/server.key\n");
    out.push_str("    permissions: \"0600\"\n");
    out.push_str("    content: |\n");
    push_block(&mut out, 6, &spec.server_key_pem);

    out.push_str("  - path: /var/lib/incus-mac/client.crt\n");
    out.push_str("    permissions: \"0644\"\n");
    out.push_str("    content: |\n");
    push_block(&mut out, 6, &spec.client_cert_pem);

    out.push_str("  - path: /var/lib/incus-mac/preseed.yaml\n");
    out.push_str("    permissions: \"0644\"\n");
    out.push_str("    content: |\n");
    push_block(&mut out, 6, &preseed);

    if !spec.mounts.is_empty() {
        out.push_str("  - path: /etc/fstab.d/incus-mac-mounts\n");
        out.push_str("    permissions: \"0644\"\n");
        out.push_str("    content: |\n");
        push_block(&mut out, 6, &fstab_mounts);
    }

    out.push_str("runcmd:\n");
    out.push_str("  - [mkdir, -p, /etc/apt/keyrings]\n");
    out.push_str(
        "  - [sh, -lc, \"curl -fsSL https://pkgs.zabbly.com/key.asc -o /etc/apt/keyrings/zabbly.asc\"]\n",
    );
    out.push_str(
        "  - [sh, -lc, \"echo 'deb [signed-by=/etc/apt/keyrings/zabbly.asc] https://pkgs.zabbly.com/incus/stable /' > /etc/apt/sources.list.d/zabbly-incus-stable.list\"]\n",
    );
    out.push_str("  - [apt-get, update]\n");
    out.push_str("  - [apt-get, install, -y, incus]\n");
    out.push_str("  - [sh, -lc, \"incus admin init --preseed < /var/lib/incus-mac/preseed.yaml\"]\n");
    out.push_str(
        "  - [incus, config, trust, add-certificate, /var/lib/incus-mac/client.crt, --name, incus-mac-client, --type, client]\n",
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
        "instance-id: incus-mac-{}\nlocal-hostname: {}\n",
        spec.hostname, spec.hostname
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ProvisionSpec {
        ProvisionSpec {
            hostname: "incus-mac".to_string(),
            bridge_cidr: "10.174.0.1/24".to_string(),
            client_cert_pem: "-----BEGIN CERTIFICATE-----\nCLIENT\n-----END CERTIFICATE-----\n"
                .to_string(),
            server_cert_pem: "-----BEGIN CERTIFICATE-----\nSERVER\n-----END CERTIFICATE-----\n"
                .to_string(),
            server_key_pem:
                "-----BEGIN PRIVATE KEY-----\nKEY\n-----END PRIVATE KEY-----\n".to_string(),
            mounts: Vec::new(),
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
            user_data.contains("incus admin init --preseed < /var/lib/incus-mac/preseed.yaml"),
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
        assert!(user_data.contains("/var/lib/incus-mac/client.crt"));
        assert!(user_data.contains("SERVER"));
        assert!(user_data.contains("KEY"));
        assert!(user_data.contains("CLIENT"));
    }

    #[test]
    fn metadata_uses_hostname_for_instance_identity() {
        let meta = render_meta_data(&spec()).expect("meta-data should render");

        assert_eq!(
            meta,
            "instance-id: incus-mac-incus-mac\nlocal-hostname: incus-mac\n"
        );
    }

    #[test]
    fn mounts_render_fstab_entries_and_mountpoint_creation() {
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

        assert!(user_data.contains("path: /etc/fstab.d/incus-mac-mounts"));
        assert!(user_data.contains("code /mnt/mac/code virtiofs defaults,_netdev 0 0"));
        assert!(user_data.contains("data /mnt/mac/data virtiofs defaults,_netdev 0 0"));
        assert!(user_data.contains("[mkdir, -p, /mnt/mac/code]"));
        assert!(user_data.contains("[mkdir, -p, /mnt/mac/data]"));
        assert!(user_data.contains("[mount, -a]"));
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
}