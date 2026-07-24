use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use clap::{Parser, Subcommand};
use malleus::{
    config::{Config, ConfigError},
    pki,
    provision::{self, ProvisionSpec},
    vm::{self, Share, VmSpec},
};

const DEFAULT_BRIDGE_CIDR: &str = "10.174.0.1/24";
const DEFAULT_VM_MAC: &str = "52:54:00:12:34:56";
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
    #[error("mount name is required when it cannot be derived from path `{0}`")]
    MissingMountName(PathBuf),
    #[error("invalid mount name `{0}`; allowed characters are letters, numbers, '-', '_', '.'")]
    InvalidMountName(String),
}

fn default_state_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".malleus")
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
    let pki_material = pki::load_or_create(&state_dir.join("pki"))?;

    let provision_spec = ProvisionSpec {
        hostname: "malleus".to_string(),
        bridge_cidr: DEFAULT_BRIDGE_CIDR.to_string(),
        client_cert_pem: pki_material.client.cert_pem.clone(),
        server_cert_pem: pki_material.server.cert_pem.clone(),
        server_key_pem: pki_material.server.key_pem.clone(),
        mounts: Vec::new(),
    };

    let user_data = provision::render_user_data(&provision_spec)?;
    let meta_data = provision::render_meta_data(&provision_spec)?;

    let user_data_path = state_dir.join("user-data");
    let meta_data_path = state_dir.join("meta-data");
    fs::write(&user_data_path, user_data)?;
    fs::write(&meta_data_path, meta_data)?;

    let spec = VmSpec {
        cpus: cfg.cpus,
        memory_mib: cfg.memory_mib,
        efi_store: state_dir.join("efi-store"),
        root_disk: state_dir.join("root.raw"),
        pool_disk: state_dir.join("pool.raw"),
        mac: DEFAULT_VM_MAC.to_string(),
        user_data: user_data_path,
        meta_data: meta_data_path,
        ready_socket: state_dir.join("ready.sock"),
        ready_port: DEFAULT_READY_PORT,
        restful_port: DEFAULT_RESTFUL_PORT,
        shares: Vec::new(),
    };

    let args = vm::build_args(&spec);
    fs::write(state_dir.join("vfkit.args"), args.join("\n"))?;

    println!("start wiring complete: {}", state_dir.display());
    Ok(())
}

fn cmd_stop() {
    println!("stop wiring complete");
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

    println!("state dir: {}", state_dir.display());
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

fn run(cli: Cli) -> Result<(), CliError> {
    let state_dir = cli.state_dir.unwrap_or_else(default_state_dir);

    match cli.command {
        Command::Start => cmd_start(&state_dir),
        Command::Stop => {
            cmd_stop();
            Ok(())
        }
        Command::Status => cmd_status(&state_dir),
        Command::Delete => cmd_delete(&state_dir),
        Command::Mount { host_path, name } => cmd_mount(host_path, name),
        Command::Unmount { name } => cmd_unmount(name),
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
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

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

    #[test]
    fn accepts_the_required_command_surface() {
        assert!(Cli::try_parse_from(["malleus", "start"]).is_ok());
        assert!(Cli::try_parse_from(["malleus", "stop"]).is_ok());
        assert!(Cli::try_parse_from(["malleus", "status"]).is_ok());
        assert!(Cli::try_parse_from(["malleus", "delete"]).is_ok());
        assert!(Cli::try_parse_from(["malleus", "mount", "/tmp/code"]).is_ok());
        assert!(Cli::try_parse_from(["malleus", "unmount", "code"]).is_ok());
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
}
