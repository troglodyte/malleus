use std::path::{Path, PathBuf};

/// A host directory exposed to the guest over virtio-fs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Share {
    /// Directory on the macOS host.
    pub host_path: PathBuf,
    /// Mount tag the guest uses to mount this share.
    pub tag: String,
}

/// Everything needed to construct a vfkit invocation.
///
/// This is deliberately a plain data record with no I/O, so the exact argument
/// vector can be asserted in tests without a macOS host or a vfkit binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmSpec {
    pub cpus: u32,
    pub memory_mib: u64,
    /// EFI variable store, created by vfkit on first boot.
    pub efi_store: PathBuf,
    /// Debian cloud image clone, grown on first boot by cloud-init.
    pub root_disk: PathBuf,
    /// Dedicated blank disk backing the btrfs Incus storage pool.
    pub pool_disk: PathBuf,
    /// Stable MAC, used to find the guest's lease in `/var/db/dhcpd_leases`.
    pub mac: String,
    pub user_data: PathBuf,
    pub meta_data: PathBuf,
    pub network_config: Option<PathBuf>,
    /// Host-side Unix socket the guest connects to when provisioning finishes.
    pub ready_socket: PathBuf,
    /// vsock port matching `ready_socket`.
    pub ready_port: u32,
    /// Port for vfkit's plain-HTTP control API on localhost.
    pub restful_port: u16,
    pub shares: Vec<Share>,
    pub serial_log: Option<PathBuf>,
}

fn path_arg(path: &Path) -> String {
    path.display().to_string()
}

/// Build the exact vfkit argument vector for `spec`.
///
/// Ordering is fixed so the invocation can be pinned by tests; vfkit itself is
/// order-insensitive for these flags.
pub fn build_args(spec: &VmSpec) -> Vec<String> {
    let mut args = vec![
        "--cpus".to_string(),
        spec.cpus.to_string(),
        "--memory".to_string(),
        spec.memory_mib.to_string(),
        "--bootloader".to_string(),
        format!("efi,variable-store={},create", path_arg(&spec.efi_store)),
        "--device".to_string(),
        format!("virtio-blk,path={},devName=vda", path_arg(&spec.root_disk)),
        "--device".to_string(),
        format!("virtio-blk,path={},devName=vdb", path_arg(&spec.pool_disk)),
        "--device".to_string(),
        format!("virtio-net,nat,mac={}", spec.mac),
        "--device".to_string(),
        format!(
            "virtio-vsock,port={},socketURL={},listen",
            spec.ready_port,
            path_arg(&spec.ready_socket)
        ),
        "--device".to_string(),
        "virtio-rng".to_string(),
    ];

    for share in &spec.shares {
        args.push("--device".to_string());
        args.push(format!(
            "virtio-fs,sharedDir={},mountTag={}",
            path_arg(&share.host_path),
            share.tag
        ));
    }

    args.push("--cloud-init".to_string());
    args.push(format!(
        "{},{}",
        path_arg(&spec.user_data),
        path_arg(&spec.meta_data)
    ));

    args.push("--restful-uri".to_string());
    args.push(format!("tcp://localhost:{}", spec.restful_port));

    // No `--kernel-cmdline` here: vfkit only accepts it together with `--kernel`
    // and `--initrd`, and we boot through EFI off the guest's own bootloader.
    // The guest still reaches the serial device below by writing to /dev/hvc0
    // directly, which the virtio-console driver provides regardless of the cmdline.
    if let Some(serial_log) = &spec.serial_log {
        args.push("--device".to_string());
        args.push(format!("virtio-serial,logFilePath={}", path_arg(serial_log)));
    }

    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn spec() -> VmSpec {
        VmSpec {
            cpus: 4,
            memory_mib: 4096,
            efi_store: PathBuf::from("/s/efi-store"),
            root_disk: PathBuf::from("/s/root.raw"),
            pool_disk: PathBuf::from("/s/pool.raw"),
            mac: "52:54:00:12:34:56".to_string(),
            user_data: PathBuf::from("/s/user-data"),
            meta_data: PathBuf::from("/s/meta-data"),
            network_config: None,
            ready_socket: PathBuf::from("/s/ready.sock"),
            ready_port: 5,
            restful_port: 8081,
            shares: Vec::new(),
            serial_log: None,
        }
    }

    #[test]
    fn builds_the_documented_vfkit_invocation() {
        let args = build_args(&spec());

        assert_eq!(
            args,
            vec![
                "--cpus",
                "4",
                "--memory",
                "4096",
                "--bootloader",
                "efi,variable-store=/s/efi-store,create",
                "--device",
                "virtio-blk,path=/s/root.raw,devName=vda",
                "--device",
                "virtio-blk,path=/s/pool.raw,devName=vdb",
                "--device",
                "virtio-net,nat,mac=52:54:00:12:34:56",
                "--device",
                "virtio-vsock,port=5,socketURL=/s/ready.sock,listen",
                "--device",
                "virtio-rng",
                "--cloud-init",
                "/s/user-data,/s/meta-data",
                "--restful-uri",
                "tcp://localhost:8081",
            ]
        );
    }

    /// vfkit groups `--kernel`, `--initrd` and `--kernel-cmdline` as all-or-nothing:
    /// setting one without the others is rejected before the VM starts with
    /// "if any flags in the group [kernel initrd kernel-cmdline] are set they must
    /// all be set". We boot via EFI and never supply a kernel or initrd, so the
    /// cmdline flag must not be emitted at all.
    #[test]
    fn efi_boot_omits_the_direct_kernel_boot_flag_group() {
        let args = build_args(&spec());

        for flag in ["--kernel-cmdline", "--kernel", "--initrd"] {
            assert!(
                !args.iter().any(|arg| arg == flag),
                "{flag} is only valid alongside a direct kernel boot, but the \
                 bootloader is EFI; vfkit refuses the whole invocation: {args:?}"
            );
        }
    }

    /// Values of every `--device` flag, in order.
    fn devices(args: &[String]) -> Vec<String> {
        args.windows(2)
            .filter(|w| w[0] == "--device")
            .map(|w| w[1].clone())
            .collect()
    }

    #[test]
    fn each_share_adds_a_virtio_fs_device() {
        let spec = VmSpec {
            shares: vec![
                Share {
                    host_path: PathBuf::from("/Users/me/code"),
                    tag: "code".to_string(),
                },
                Share {
                    host_path: PathBuf::from("/Users/me/data"),
                    tag: "data".to_string(),
                },
            ],
            ..spec()
        };

        let devices = devices(&build_args(&spec));

        assert!(
            devices.contains(&"virtio-fs,sharedDir=/Users/me/code,mountTag=code".to_string()),
            "missing virtio-fs device for the code share, got: {devices:?}"
        );
        assert!(
            devices.contains(&"virtio-fs,sharedDir=/Users/me/data,mountTag=data".to_string()),
            "missing virtio-fs device for the data share, got: {devices:?}"
        );
    }

    #[test]
    fn share_devices_are_emitted_before_cloud_init_in_input_order() {
        let spec = VmSpec {
            shares: vec![
                Share {
                    host_path: PathBuf::from("/Users/me/alpha"),
                    tag: "alpha".to_string(),
                },
                Share {
                    host_path: PathBuf::from("/Users/me/beta"),
                    tag: "beta".to_string(),
                },
            ],
            ..spec()
        };

        let args = build_args(&spec);

        let alpha_idx = args
            .iter()
            .position(|arg| arg == "virtio-fs,sharedDir=/Users/me/alpha,mountTag=alpha")
            .expect("alpha share device should exist");
        let beta_idx = args
            .iter()
            .position(|arg| arg == "virtio-fs,sharedDir=/Users/me/beta,mountTag=beta")
            .expect("beta share device should exist");
        let cloud_init_idx = args
            .iter()
            .position(|arg| arg == "--cloud-init")
            .expect("--cloud-init should exist");

        assert!(alpha_idx < beta_idx, "share order should be stable: {args:?}");
        assert!(
            beta_idx < cloud_init_idx,
            "all share devices should precede --cloud-init: {args:?}"
        );
    }

    #[test]
    fn serial_log_adds_a_virtio_serial_device() {
        let spec = VmSpec {
            serial_log: Some(PathBuf::from("/tmp/guest.log")),
            ..spec()
        };

        let args = build_args(&spec);
        let devices = devices(&args);

        assert!(
            devices.contains(&"virtio-serial,logFilePath=/tmp/guest.log".to_string()),
            "missing virtio-serial device for the log, got: {devices:?}"
        );
    }
}
