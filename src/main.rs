use std::{
    ffi::OsString,
    fs,
    net::Ipv4Addr,
    path::{Path, PathBuf},
};

use clap::{Parser, Subcommand};
use malleus::{
    config::{Config, ConfigError},
    image,
    leases,
    pki,
    provision::{self, ProvisionSpec},
    vm::{self, Share, VmSpec},
};

const DEFAULT_BRIDGE_CIDR: &str = "10.174.0.1/24";
const DEFAULT_VM_MAC: &str = "52:54:00:12:34:56";
const DEFAULT_INCUS_REMOTE_NAME: &str = "malleus";
const DEFAULT_INCUS_HTTPS_PORT: u16 = 8443;
const DEFAULT_DHCP_LEASES_PATH: &str = "/var/db/dhcpd_leases";
const DEFAULT_READY_PORT: u32 = 5;
const DEFAULT_RESTFUL_PORT: u16 = 8444;

#[derive(Debug, Parser)]
#[command(name = "malleus", version, about = "Run Incus from macOS through a managed Linux VM")]
struct Cli {
    /// Path to the malleus state directory.
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Boot/reconcile VM-side artifacts and local trust material.
    Start,
    /// Stop the managed VM.
    Stop,
    /// Show a brief status summary.
    Status,
    /// Remove managed state.
    Delete,
    /// Register a host path as a virtio-fs share tag.
    Mount {
        host_path: PathBuf,
        name: Option<String>,
    },
    /// Remove a registered virtio-fs share tag.
    Unmount { name: String },
    /// Configure local Incus credentials and the `malleus` remote.
    Autoconfigure {
        /// VM IPv4 address. If omitted, resolves from DHCP leases.
        #[arg(long)]
        vm_ip: Option<Ipv4Addr>,
        /// Incus remote name to add/switch.
        #[arg(long, default_value = DEFAULT_INCUS_REMOTE_NAME)]
        remote_name: String,
        /// Incus config directory (defaults to $INCUS_CONF or ~/.config/incus-malleus).
        #[arg(long)]
        incus_conf: Option<PathBuf>,
    },
}

#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error(transparent)]
    Clap(#[from] clap::Error),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse config TOML `{path}`: {source}")]
    ConfigParse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("invalid VM configuration: {0}")]
    ConfigValidation(#[from] ConfigError),
    #[error("PKI error: {0}")]
    Pki(#[from] pki::PkiError),
    #[error("provisioning render error: {0}")]
    Provision(#[from] provision::ProvisionError),
    #[error("image error: {0}")]
    Image(#[from] image::ImageError),
    #[error("mount name is required when it cannot be derived from path `{0}`")]
    MissingMountName(PathBuf),
    #[error("invalid mount name `{0}`; allowed characters are letters, numbers, '-', '_', '.'")]
    InvalidMountName(String),
    #[error("invalid remote name `{0}`; allowed characters are letters, numbers, '-', '_', '.'")]
    InvalidRemoteName(String),
    #[error("missing PKI material in `{0}`; run `malleus start` first")]
    MissingPkiMaterial(PathBuf),
    #[error("unable to find VM IP for MAC `{mac}` or name `{name}`. Searched: {searched_paths}. Reason: {reason}; pass `--vm-ip` or ensure the VM is running")]
    MissingVmIp {
        searched_paths: String,
        mac: String,
        name: String,
        reason: String,
    },
    #[cfg_attr(test, allow(dead_code))]
    #[error("vfkit not found in PATH; please install it or ensure it is in your PATH")]
    VfkitNotFound,
    #[cfg_attr(test, allow(dead_code))]
    #[error("failed to execute `vfkit`: {0}")]
    VfkitExec(std::io::Error),
    #[error("failed to execute `incus {args}`: {source}")]
    IncusExec {
        args: String,
        source: std::io::Error,
    },
    #[error("`incus {args}` failed with status {status}: {stderr}")]
    IncusFailed {
        args: String,
        status: i32,
        stderr: String,
    },
}

fn default_state_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".malleus")
}

fn default_incus_conf_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("INCUS_CONF")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    {
        return path;
    }

    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/incus-malleus")
}

fn load_config(state_dir: &Path) -> Result<Config, CliError> {
    let path = state_dir.join("config.toml");
    if !path.exists() {
        let cfg = Config::default();
        cfg.validate()?;
        return Ok(cfg);
    }

    let raw = fs::read_to_string(&path)?;
    let cfg: Config = toml::from_str(&raw).map_err(|source| CliError::ConfigParse {
        path: path.clone(),
        source,
    })?;

    cfg.validate()?;
    Ok(cfg)
}

fn valid_mount_name(name: &str) -> bool {
    !name.trim().is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.')
}

fn derive_mount_name(host_path: &Path) -> Result<String, CliError> {
    host_path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| CliError::MissingMountName(host_path.to_path_buf()))
}

fn cmd_start(state_dir: &Path) -> Result<(), CliError> {
    fs::create_dir_all(state_dir)?;

    let cfg = load_config(state_dir)?;
    #[cfg(not(test))]
    image::ensure_images(state_dir, &cfg)?;
    let pki_material = pki::load_or_create(&state_dir.join("pki"))?;

    let mounts = Vec::new();
    // In a future version, we would load registered mounts here.

    let provision_spec = ProvisionSpec {
        hostname: "malleus".to_string(),
        bridge_cidr: DEFAULT_BRIDGE_CIDR.to_string(),
        client_cert_pem: pki_material.client.cert_pem.clone(),
        server_cert_pem: pki_material.server.cert_pem.clone(),
        server_key_pem: pki_material.server.key_pem.clone(),
        mounts,
        state_tag: Some("malleus-state".to_string()),
    };

    let user_data = provision::render_user_data(&provision_spec)?;
    let meta_data = provision::render_meta_data(&provision_spec)?;

    let user_data_path = state_dir.join("user-data");
    let meta_data_path = state_dir.join("meta-data");
    fs::write(&user_data_path, user_data)?;
    fs::write(&meta_data_path, meta_data)?;

    let mut shares = Vec::new();
    // In a future version, we would load registered mounts here.
    shares.push(Share {
        host_path: state_dir.to_path_buf(),
        tag: "malleus-state".to_string(),
    });

    let spec = VmSpec {
        cpus: cfg.cpus,
        memory_mib: cfg.memory_mib,
        efi_store: state_dir.join("efi-store"),
        root_disk: state_dir.join("root.raw"),
        pool_disk: state_dir.join("pool.raw"),
        mac: DEFAULT_VM_MAC.to_string(),
        user_data: user_data_path,
        meta_data: meta_data_path,
        network_config: None,
        ready_socket: state_dir.join("ready.sock"),
        ready_port: DEFAULT_READY_PORT,
        restful_port: DEFAULT_RESTFUL_PORT,
        shares,
        serial_log: Some(state_dir.join("vfkit.guest.log")),
    };

    let args = vm::build_args(&spec);
    fs::write(state_dir.join("vfkit.args"), args.join("\n"))?;
    start_vm(state_dir, &args)?;

    println!("start wiring complete: {}", state_dir.display());
    Ok(())
}

fn start_vm(state_dir: &Path, args: &[String]) -> Result<(), CliError> {
    #[cfg(not(test))]
    {
        use std::io::{self, Write};

        println!("starting VM via vfkit...");
        loop {
            match std::process::Command::new("vfkit")
                .args(args)
                .stdin(std::process::Stdio::null())
                .stdout(fs::File::create(state_dir.join("vfkit.log"))?)
                .stderr(fs::File::create(state_dir.join("vfkit.err"))?)
                .spawn()
            {
                Ok(child) => {
                    fs::write(state_dir.join("vfkit.pid"), child.id().to_string())?;
                    println!("VM is booting (PID: {})", child.id());
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    let has_brew = std::process::Command::new("brew")
                        .arg("--version")
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false);

                    if !has_brew {
                        return Err(CliError::VfkitNotFound);
                    }

                    print!("vfkit is not installed. Would you like to install it via Homebrew? [y/N] ");
                    io::stdout().flush()?;

                    let mut input = String::new();
                    io::stdin().read_line(&mut input)?;

                    if input.trim().to_lowercase() == "y" {
                        println!("Installing vfkit via Homebrew...");
                        let status = std::process::Command::new("brew")
                            .args(["install", "vfkit"])
                            .status()?;

                        if status.success() {
                            println!("vfkit installed successfully.");
                            continue; // Try starting again
                        } else {
                            return Err(CliError::VfkitExec(io::Error::new(
                                io::ErrorKind::Other,
                                "brew install vfkit failed",
                            )));
                        }
                    } else {
                        return Err(CliError::VfkitNotFound);
                    }
                }
                Err(e) => return Err(CliError::VfkitExec(e)),
            }
        }
    }

    #[cfg(test)]
    {
        let _ = state_dir;
        let _ = args;
    }

    Ok(())
}

fn cmd_stop(state_dir: &Path) -> Result<(), CliError> {
    let pid_path = state_dir.join("vfkit.pid");
    if !pid_path.exists() {
        println!("no VM PID file found at {}; assuming already stopped", pid_path.display());
        return Ok(());
    }

    let pid_str = fs::read_to_string(&pid_path)?;
    let pid: u32 = pid_str.trim().parse().map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid PID file content")
    })?;

    println!("stopping VM (PID: {})...", pid);

    #[cfg(not(test))]
    {
        // On macOS/Unix, we use kill
        let status = std::process::Command::new("kill")
            .arg(pid.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        if let Ok(s) = status {
            if s.success() {
                println!("sent stop signal to VM");
            } else {
                println!("failed to stop VM (it may have already exited)");
            }
            let _ = fs::remove_file(pid_path);
            let _ = fs::remove_file(state_dir.join("ready.sock"));
        }
    }

    #[cfg(test)]
    {
        let _ = pid;
        let _ = fs::remove_file(pid_path);
    }

    Ok(())
}

fn cmd_status(state_dir: &Path) -> Result<(), CliError> {
    let cfg = load_config(state_dir)?;
    let pki_paths = pki::pki_paths(&state_dir.join("pki"));
    let pki_ready = [
        pki_paths.client_cert,
        pki_paths.client_key,
        pki_paths.server_cert,
        pki_paths.server_key,
    ]
    .iter()
    .all(|path| path.exists());

    let pid_path = state_dir.join("vfkit.pid");
    let vm_status = if let Ok(pid_str) = fs::read_to_string(&pid_path) {
        if let Ok(pid) = pid_str.trim().parse::<u32>() {
            let is_alive = std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);

            if is_alive {
                format!("running (PID: {})", pid)
            } else {
                "stopped (stale PID file)".to_string()
            }
        } else {
            "stopped (invalid PID file)".to_string()
        }
    } else {
        "stopped".to_string()
    };

    println!("state dir: {}", state_dir.display());
    println!("vm: {}", vm_status);
    println!("cpus: {}", cfg.cpus);
    println!("memory_mib: {}", cfg.memory_mib);
    println!("pki: {}", if pki_ready { "present" } else { "missing" });
    Ok(())
}

fn cmd_delete(state_dir: &Path) -> Result<(), CliError> {
    if state_dir.exists() {
        fs::remove_dir_all(state_dir)?;
    }
    println!("deleted state: {}", state_dir.display());
    Ok(())
}

fn cmd_mount(host_path: PathBuf, name: Option<String>) -> Result<(), CliError> {
    let mount_name = match name {
        Some(name) => name,
        None => derive_mount_name(&host_path)?,
    };

    if !valid_mount_name(&mount_name) {
        return Err(CliError::InvalidMountName(mount_name));
    }

    let _share = Share {
        host_path: host_path.clone(),
        tag: mount_name.clone(),
    };

    println!(
        "mount wiring complete: {} -> {}",
        host_path.display(),
        mount_name
    );
    Ok(())
}

fn cmd_unmount(name: String) -> Result<(), CliError> {
    if !valid_mount_name(&name) {
        return Err(CliError::InvalidMountName(name));
    }

    println!("unmount wiring complete: {}", name);
    Ok(())
}

trait IncusRunner {
    fn run(&mut self, incus_conf: &Path, args: &[&str]) -> Result<String, CliError>;
}

struct ProcessIncusRunner;

impl IncusRunner for ProcessIncusRunner {
    fn run(&mut self, incus_conf: &Path, args: &[&str]) -> Result<String, CliError> {
        let output = std::process::Command::new("incus")
            .env("INCUS_CONF", incus_conf)
            .args(args)
            .output()
            .map_err(|source| CliError::IncusExec {
                args: args.join(" "),
                source,
            })?;

        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
        }

        Err(CliError::IncusFailed {
            args: args.join(" "),
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    }
}

fn remote_exists(remote_list_csv: &str, remote_name: &str) -> bool {
    remote_list_csv.lines().any(|line| {
        let name = line
            .split(',')
            .next()
            .map(str::trim)
            .unwrap_or("")
            .trim_end_matches(" (current)");

        !name.is_empty() && !name.eq_ignore_ascii_case("name") && name == remote_name
    })
}

fn is_vm_running(state_dir: &Path) -> bool {
    let pid_path = state_dir.join("vfkit.pid");
    if let Ok(pid_str) = fs::read_to_string(&pid_path) {
        if let Ok(pid) = pid_str.trim().parse::<u32>() {
            let status = std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            return matches!(status, Ok(s) if s.success());
        }
    }
    false
}

fn query_vfkit_ip(port: u16) -> Option<Ipv4Addr> {
    let url = format!("http://localhost:{}/vm/inspect", port);
    eprintln!("debug: querying vfkit REST API at {}", url);
    let output = std::process::Command::new("curl")
        .args(["-s", &url])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let v: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    eprintln!("debug: vfkit REST API response: {:?}", v);

    // Try multiple possible paths for the IP
    let ip_str = v
        .get("vm")
        .and_then(|vm| vm.get("ip"))
        .and_then(|ip| ip.as_str())
        .or_else(|| v.get("ip").and_then(|ip| ip.as_str()))
        .or_else(|| {
            v.get("network")
                .and_then(|n| n.get("ip"))
                .and_then(|ip| ip.as_str())
        });

    ip_str.and_then(|s| s.parse().ok()).or_else(|| {
        v.get("devices")
            .and_then(|d| d.as_array())
            .and_then(|arr| {
                arr.iter().find_map(|dev| {
                    if dev.get("kind").and_then(|k| k.as_str()) == Some("virtionet") {
                        dev.get("ip").and_then(|ip| ip.as_str())
                    } else {
                        None
                    }
                })
            })
            .and_then(|s| s.parse().ok())
    })
}

fn query_vsock_ip(ready_sock: &Path) -> Option<Ipv4Addr> {
    use std::io::Read;
    use std::os::unix::net::UnixStream;

    eprintln!(
        "debug: attempting vsock IP discovery via {}",
        ready_sock.display()
    );
    let mut stream = UnixStream::connect(ready_sock).ok()?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_millis(1000)))
        .ok()?;

    let mut buf = String::new();
    if let Ok(_) = stream.read_to_string(&mut buf) {
        let ip = buf.split_whitespace().next()?.parse().ok();
        if let Some(ip) = ip {
            eprintln!("debug: received IP via vsock: {}", ip);
            return Some(ip);
        }
    }
    None
}

fn resolve_vm_ip(
    vm_ip: Option<Ipv4Addr>,
    lease_path: &Path,
    state_dir: &Path,
) -> Result<Ipv4Addr, CliError> {
    if let Some(vm_ip) = vm_ip {
        eprintln!("debug: using explicitly provided VM IP: {}", vm_ip);
        return Ok(vm_ip);
    }

    let paths = [lease_path, Path::new("/var/db/dhcp_leases")];
    let mut searched = vec![
        "vfkit REST API".to_string(),
        "vsock readiness signal".to_string(),
        "virtio-fs state share".to_string(),
        "guest serial log".to_string(),
    ];

    let start_time = std::time::Instant::now();
    let timeout = if cfg!(test) {
        std::time::Duration::from_secs(1)
    } else {
        std::time::Duration::from_secs(60)
    };

    eprintln!("debug: resolving VM IP (timeout: {:?})...", timeout);

    loop {
        // First check if the VM is even running
        if !is_vm_running(state_dir) {
            let ready_sock = state_dir.join("ready.sock");
            let mut reason = "VM is not running (checked PID file)".to_string();
            if !ready_sock.exists() {
                reason.push_str(&format!(
                    "; VM readiness socket `{}` missing",
                    ready_sock.display()
                ));
            }

            // Try to capture last error from vfkit.err
            if let Ok(err_log) = fs::read_to_string(state_dir.join("vfkit.err")) {
                let last_lines: Vec<&str> = err_log.lines().rev().take(3).collect();
                if !last_lines.is_empty() {
                    reason.push_str("; last hypervisor errors: ");
                    reason.push_str(&last_lines.join(" | "));
                }
            }

            return Err(CliError::MissingVmIp {
                searched_paths: "PID check".to_string(),
                mac: DEFAULT_VM_MAC.to_string(),
                name: "malleus".to_string(),
                reason,
            });
        }

        // Try vfkit REST API first as it is the most direct
        if let Some(ip) = query_vfkit_ip(DEFAULT_RESTFUL_PORT) {
            eprintln!("debug: found VM IP via vfkit REST API: {}", ip);
            return Ok(ip);
        }

        // Try vsock readiness signal
        let ready_sock = state_dir.join("ready.sock");
        if let Some(ip) = query_vsock_ip(&ready_sock) {
            eprintln!("debug: found VM IP via vsock readiness signal: {}", ip);
            return Ok(ip);
        } else {
            eprintln!("debug: vsock IP discovery at {} returned no IP", ready_sock.display());
        }

        // Try virtio-fs state share (most reliable fallback)
        let guest_ip_path = state_dir.join("guest-ip");
        if let Ok(ip_str) = fs::read_to_string(&guest_ip_path) {
            if let Ok(ip) = ip_str.trim().parse::<Ipv4Addr>() {
                eprintln!("debug: found VM IP via virtio-fs state share: {}", ip);
                return Ok(ip);
            }
        } else {
            eprintln!("debug: virtio-fs state share at {} not found or empty", guest_ip_path.display());
        }

        // Try parsing guest serial log for "Reported IP" lines
        let guest_log_path = state_dir.join("vfkit.guest.log");
        if let Ok(content) = fs::read_to_string(&guest_log_path) {
            for line in content.lines().rev() {
                if let Some(pos) = line.find("Reported IP ") {
                    let part = &line[pos + 12..];
                    let ip_str = part.split_whitespace().next().unwrap_or("");
                    if let Ok(ip) = ip_str.parse::<Ipv4Addr>() {
                        eprintln!("debug: found VM IP via guest serial log: {}", ip);
                        return Ok(ip);
                    }
                }
            }
        } else {
            eprintln!("debug: guest serial log at {} not found or inaccessible", guest_log_path.display());
        }

        let mut reasons = Vec::new();
        for path in paths {
            if !searched.contains(&path.display().to_string()) {
                searched.push(path.display().to_string());
                eprintln!("debug: searching for lease in {}", path.display());
            }

            if let Ok(lease_contents) = fs::read_to_string(path) {
                if let Some(ip) =
                    leases::find_lease(&lease_contents, DEFAULT_VM_MAC, Some("malleus"))
                {
                    eprintln!(
                        "debug: found lease for MAC {} or name malleus: {}",
                        DEFAULT_VM_MAC, ip
                    );
                    return Ok(ip);
                }
                reasons.push(format!("{}: no matching record", path.display()));
            } else {
                reasons.push(format!("{}: not found or inaccessible", path.display()));
            }
        }

        // Fallback to ARP table
        if !searched.contains(&"host ARP table".to_string()) {
            searched.push("host ARP table".to_string());
            eprintln!("debug: searching in host ARP table");
        }
        let arp_output = std::process::Command::new("arp").args(["-an"]).output();
        match arp_output {
            Ok(output) if output.status.success() => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Some(ip) = leases::find_in_arp(&stdout, DEFAULT_VM_MAC) {
                    return Ok(ip);
                }
                reasons.push("ARP table: no entry for MAC".to_string());
            }
            _ => {
                reasons.push("ARP table: command failed or unavailable".to_string());
            }
        }

        if start_time.elapsed() >= timeout {
            // Try to capture last guest logs
            if let Ok(guest_log) = fs::read_to_string(state_dir.join("vfkit.guest.log")) {
                let last_lines: Vec<&str> = guest_log.lines().rev().take(5).collect();
                if !last_lines.is_empty() {
                    reasons.push(format!("last guest logs: {}", last_lines.join(" | ")));
                } else {
                    reasons.push("guest serial log is empty".to_string());
                }
            } else {
                reasons.push("guest serial log not found".to_string());
            }

            // Also include virtio-fs status
            let guest_ip_path = state_dir.join("guest-ip");
            if guest_ip_path.exists() {
                reasons.push("virtio-fs guest-ip file exists but contains no valid IP".to_string());
            } else {
                reasons.push("virtio-fs guest-ip file missing".to_string());
            }

            return Err(CliError::MissingVmIp {
                searched_paths: searched.join(", "),
                mac: DEFAULT_VM_MAC.to_string(),
                name: "malleus".to_string(),
                reason: reasons.join("; "),
            });
        }

        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

fn cmd_autoconfigure_with_runner<R: IncusRunner>(
    state_dir: &Path,
    vm_ip: Option<Ipv4Addr>,
    remote_name: String,
    incus_conf: Option<PathBuf>,
    lease_path: &Path,
    runner: &mut R,
) -> Result<(), CliError> {
    if !valid_mount_name(&remote_name) {
        return Err(CliError::InvalidRemoteName(remote_name));
    }

    let pki_dir = state_dir.join("pki");
    let pki_paths = pki::pki_paths(&pki_dir);
    if !pki_paths.client_cert.exists() || !pki_paths.client_key.exists() {
        return Err(CliError::MissingPkiMaterial(pki_dir));
    }

    let incus_conf_dir = incus_conf.unwrap_or_else(default_incus_conf_dir);
    fs::create_dir_all(&incus_conf_dir)?;
    fs::copy(&pki_paths.client_cert, incus_conf_dir.join("client.crt"))?;
    fs::copy(&pki_paths.client_key, incus_conf_dir.join("client.key"))?;

    if !is_vm_running(state_dir) {
        println!("VM is not running; starting it now...");
        cmd_start(state_dir)?;
    }

    let vm_ip = resolve_vm_ip(vm_ip, lease_path, state_dir)?;
    let remote_url = format!("https://{vm_ip}:{DEFAULT_INCUS_HTTPS_PORT}");

    let remote_list = runner.run(&incus_conf_dir, &["remote", "list", "--format", "csv"])?;
    if !remote_exists(&remote_list, &remote_name) {
        runner.run(
            &incus_conf_dir,
            &[
                "remote",
                "add",
                &remote_name,
                &remote_url,
                "--accept-certificate",
            ],
        )?;
    }

    runner.run(&incus_conf_dir, &["remote", "switch", &remote_name])?;

    println!("autoconfigure wiring complete: {}", incus_conf_dir.display());
    println!("INCUS_CONF={}", incus_conf_dir.display());
    println!("remote `{remote_name}` -> {remote_url}");
    Ok(())
}

fn cmd_autoconfigure(
    state_dir: &Path,
    vm_ip: Option<Ipv4Addr>,
    remote_name: String,
    incus_conf: Option<PathBuf>,
) -> Result<(), CliError> {
    let mut runner = ProcessIncusRunner;
    cmd_autoconfigure_with_runner(
        state_dir,
        vm_ip,
        remote_name,
        incus_conf,
        Path::new(DEFAULT_DHCP_LEASES_PATH),
        &mut runner,
    )
}

fn run(cli: Cli) -> Result<(), CliError> {
    let state_dir = cli.state_dir.unwrap_or_else(default_state_dir);

    match cli.command {
        Command::Start => cmd_start(&state_dir),
        Command::Stop => cmd_stop(&state_dir),
        Command::Status => cmd_status(&state_dir),
        Command::Delete => cmd_delete(&state_dir),
        Command::Mount { host_path, name } => cmd_mount(host_path, name),
        Command::Unmount { name } => cmd_unmount(name),
        Command::Autoconfigure {
            vm_ip,
            remote_name,
            incus_conf,
        } => cmd_autoconfigure(&state_dir, vm_ip, remote_name, incus_conf),
    }
}

fn run_with_args<I, T>(args: I) -> Result<(), CliError>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = Cli::try_parse_from(args)?;
    run(cli)
}

fn main() {
    if let Err(err) = run_with_args(std::env::args_os()) {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        net::Ipv4Addr,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    #[derive(Default)]
    struct FakeIncusRunner {
        remote_list_csv: String,
        calls: Vec<(PathBuf, Vec<String>)>,
    }

    impl IncusRunner for FakeIncusRunner {
        fn run(&mut self, incus_conf: &Path, args: &[&str]) -> Result<String, CliError> {
            self.calls.push((
                incus_conf.to_path_buf(),
                args.iter().map(|arg| (*arg).to_string()).collect(),
            ));

            if args == ["remote", "list", "--format", "csv"] {
                return Ok(self.remote_list_csv.clone());
            }

            Ok(String::new())
        }
    }

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(prefix: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);

            let mut path = std::env::temp_dir();
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock should be after epoch")
                .as_nanos();
            let seq = NEXT.fetch_add(1, Ordering::Relaxed);

            path.push(format!("malleus-cli-{prefix}-{}-{stamp}-{seq}", std::process::id()));
            fs::create_dir_all(&path).expect("temp test directory should be creatable");

            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn run_with_state_dir(temp: &TempDir, command: &[&str]) -> Result<(), CliError> {
        let mut args = vec![
            OsString::from("malleus"),
            OsString::from("--state-dir"),
            temp.path.clone().into_os_string(),
        ];
        args.extend(command.iter().map(OsString::from));

        run_with_args(args)
    }

    fn write_test_pki(state_dir: &Path) {
        let pki_dir = state_dir.join("pki");
        fs::create_dir_all(&pki_dir).expect("pki directory should be creatable");
        fs::write(pki_dir.join("client.crt"), "CLIENT")
            .expect("client cert fixture should be writable");
        fs::write(pki_dir.join("client.key"), "KEY").expect("client key fixture should be writable");
    }

    fn write_test_pid(state_dir: &Path) {
        fs::write(state_dir.join("vfkit.pid"), std::process::id().to_string())
            .expect("mock pid file should be writable");
    }

    #[test]
    fn accepts_the_required_command_surface() {
        assert!(Cli::try_parse_from(["malleus", "start"]).is_ok());
        assert!(Cli::try_parse_from(["malleus", "stop"]).is_ok());
        assert!(Cli::try_parse_from(["malleus", "status"]).is_ok());
        assert!(Cli::try_parse_from(["malleus", "delete"]).is_ok());
        assert!(Cli::try_parse_from(["malleus", "mount", "/tmp/code"]).is_ok());
        assert!(Cli::try_parse_from(["malleus", "unmount", "code"]).is_ok());
        assert!(Cli::try_parse_from(["malleus", "autoconfigure"]).is_ok());
        assert!(
            Cli::try_parse_from(["malleus", "autoconfigure", "--vm-ip", "192.168.64.5"])
                .is_ok()
        );
    }

    #[test]
    fn start_creates_bootstrap_artifacts() {
        let temp = TempDir::new("start");

        run_with_state_dir(&temp, &["start"]).expect("start should succeed");

        let pki_dir = temp.path.join("pki");
        let paths = pki::pki_paths(&pki_dir);
        assert!(paths.client_cert.exists());
        assert!(paths.client_key.exists());
        assert!(paths.server_cert.exists());
        assert!(paths.server_key.exists());
        assert!(temp.path.join("user-data").exists());
        assert!(temp.path.join("meta-data").exists());
        assert!(temp.path.join("vfkit.args").exists());
    }

    #[test]
    fn mount_rejects_invalid_name() {
        let temp = TempDir::new("mount-invalid");

        let err = run_with_state_dir(&temp, &["mount", "/tmp/code", "bad/tag"])
            .expect_err("invalid mount name should fail");

        assert!(matches!(err, CliError::InvalidMountName(_)));
    }

    #[test]
    fn autoconfigure_copies_certs_and_wires_remote() {
        let temp = TempDir::new("autoconfigure-copy");
        write_test_pki(&temp.path);
        write_test_pid(&temp.path);

        let incus_conf = temp.path.join("incus-conf");
        let lease_path = temp.path.join("dhcpd_leases");
        fs::write(&lease_path, "")
            .expect("lease fixture should be writable even when vm ip is provided");

        let mut runner = FakeIncusRunner::default();

        cmd_autoconfigure_with_runner(
            &temp.path,
            Some(Ipv4Addr::new(192, 168, 64, 5)),
            "malleus".to_string(),
            Some(incus_conf.clone()),
            &lease_path,
            &mut runner,
        )
        .expect("autoconfigure should succeed");

        assert_eq!(
            fs::read_to_string(incus_conf.join("client.crt"))
                .expect("configured client cert should be readable"),
            "CLIENT"
        );
        assert_eq!(
            fs::read_to_string(incus_conf.join("client.key"))
                .expect("configured client key should be readable"),
            "KEY"
        );

        assert_eq!(runner.calls.len(), 3);
        assert_eq!(runner.calls[0].1, vec!["remote", "list", "--format", "csv"]);
        assert_eq!(
            runner.calls[1].1,
            vec![
                "remote",
                "add",
                "malleus",
                "https://192.168.64.5:8443",
                "--accept-certificate",
            ]
        );
        assert_eq!(runner.calls[2].1, vec!["remote", "switch", "malleus"]);
        assert!(runner.calls.iter().all(|(path, _)| path == &incus_conf));
    }

    #[test]
    fn autoconfigure_uses_lease_when_vm_ip_is_not_passed() {
        let temp = TempDir::new("autoconfigure-lease");
        write_test_pki(&temp.path);
        write_test_pid(&temp.path);

        let incus_conf = temp.path.join("incus-conf");
        let lease_path = temp.path.join("dhcpd_leases");
        fs::write(
            &lease_path,
            "{\n\tip_address=192.168.64.5\n\thw_address=1,52:54:0:12:34:56\n}\n",
        )
        .expect("lease fixture should be writable");

        let mut runner = FakeIncusRunner::default();

        cmd_autoconfigure_with_runner(
            &temp.path,
            None,
            "malleus".to_string(),
            Some(incus_conf),
            &lease_path,
            &mut runner,
        )
        .expect("autoconfigure should resolve vm ip from leases");

        assert_eq!(
            runner.calls[1].1,
            vec![
                "remote",
                "add",
                "malleus",
                "https://192.168.64.5:8443",
                "--accept-certificate",
            ]
        );
    }

    #[test]
    fn autoconfigure_skips_remote_add_when_remote_exists() {
        let temp = TempDir::new("autoconfigure-existing");
        write_test_pki(&temp.path);
        write_test_pid(&temp.path);

        let incus_conf = temp.path.join("incus-conf");
        let lease_path = temp.path.join("dhcpd_leases");
        fs::write(&lease_path, "")
            .expect("lease fixture should be writable even when vm ip is provided");

        let mut runner = FakeIncusRunner {
            remote_list_csv: "malleus,https://192.168.64.5:8443\n".to_string(),
            ..FakeIncusRunner::default()
        };

        cmd_autoconfigure_with_runner(
            &temp.path,
            Some(Ipv4Addr::new(192, 168, 64, 5)),
            "malleus".to_string(),
            Some(incus_conf),
            &lease_path,
            &mut runner,
        )
        .expect("autoconfigure should succeed when remote already exists");

        assert_eq!(runner.calls.len(), 2);
        assert_eq!(runner.calls[0].1, vec!["remote", "list", "--format", "csv"]);
        assert_eq!(runner.calls[1].1, vec!["remote", "switch", "malleus"]);
    }

    #[test]
    fn autoconfigure_requires_existing_pki_material() {
        let temp = TempDir::new("autoconfigure-missing-pki");
        let mut runner = FakeIncusRunner::default();

        let err = cmd_autoconfigure_with_runner(
            &temp.path,
            Some(Ipv4Addr::new(192, 168, 64, 5)),
            "malleus".to_string(),
            Some(temp.path.join("incus-conf")),
            Path::new("/unused"),
            &mut runner,
        )
        .expect_err("autoconfigure should fail when pki is missing");

        assert!(matches!(err, CliError::MissingPkiMaterial(_)));
    }

    #[test]
    fn autoconfigure_requires_vm_ip_when_lease_has_no_match() {
        let temp = TempDir::new("autoconfigure-missing-ip");
        write_test_pki(&temp.path);
        write_test_pid(&temp.path);

        let lease_path = temp.path.join("dhcpd_leases");
        fs::write(
            &lease_path,
            "{\n\tip_address=192.168.64.9\n\thw_address=1,aa:bb:cc:dd:ee:ff\n}\n",
        )
        .expect("lease fixture should be writable");

        let mut runner = FakeIncusRunner::default();

        let err = cmd_autoconfigure_with_runner(
            &temp.path,
            None,
            "malleus".to_string(),
            Some(temp.path.join("incus-conf")),
            &lease_path,
            &mut runner,
        )
        .expect_err("autoconfigure should fail when lease lookup has no matching MAC");

        assert!(matches!(err, CliError::MissingVmIp { .. }));
    }

    #[test]
    fn autoconfigure_requires_vm_ip_when_lease_file_is_missing() {
        let temp = TempDir::new("autoconfigure-missing-lease-file");
        write_test_pki(&temp.path);
        write_test_pid(&temp.path);

        let mut runner = FakeIncusRunner::default();

        let err = cmd_autoconfigure_with_runner(
            &temp.path,
            None,
            "malleus".to_string(),
            Some(temp.path.join("incus-conf")),
            &temp.path.join("missing_dhcpd_leases"),
            &mut runner,
        )
        .expect_err("autoconfigure should fail when lease file is missing");

        assert!(matches!(err, CliError::MissingVmIp { .. }));
    }

    #[test]
    fn autoconfigure_rejects_invalid_remote_name() {
        let temp = TempDir::new("autoconfigure-bad-remote");
        write_test_pki(&temp.path);

        let mut runner = FakeIncusRunner::default();

        let err = cmd_autoconfigure_with_runner(
            &temp.path,
            Some(Ipv4Addr::new(192, 168, 64, 5)),
            "bad/name".to_string(),
            Some(temp.path.join("incus-conf")),
            Path::new("/unused"),
            &mut runner,
        )
        .expect_err("invalid remote name should fail");

        assert!(matches!(err, CliError::InvalidRemoteName(_)));
    }
}
