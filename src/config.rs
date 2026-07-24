use serde::{Deserialize, Serialize};

/// User-editable settings for the incus-mac VM, read from `~/.incus-mac/config.toml`.
///
/// Every field is optional in the file; omitted fields fall back to [`Config::default`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    /// Virtual CPUs assigned to the guest.
    pub cpus: u32,
    /// Guest RAM in MiB.
    pub memory_mib: u64,
    /// Size of the root disk in GiB. Grown on first boot by cloud-init.
    pub root_disk_gib: u64,
    /// Size of the dedicated btrfs storage-pool disk in GiB.
    pub pool_disk_gib: u64,
}

/// A setting that would produce a VM that cannot boot or run Incus usefully.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("cpus must be at least 1, got {0}")]
    Cpus(u32),
}

impl Config {
    /// Reject settings that would fail late and confusingly at boot time.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.cpus == 0 {
            return Err(ConfigError::Cpus(self.cpus));
        }
        Ok(())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cpus: 4,
            memory_mib: 4096,
            root_disk_gib: 20,
            pool_disk_gib: 50,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_uses_defaults() {
        let cfg: Config = toml::from_str("").expect("an empty config should parse");

        assert_eq!(cfg.cpus, 4);
        assert_eq!(cfg.memory_mib, 4096);
        assert_eq!(cfg.root_disk_gib, 20);
        assert_eq!(cfg.pool_disk_gib, 50);
    }

    #[test]
    fn zero_cpus_is_rejected() {
        let cfg = Config {
            cpus: 0,
            ..Config::default()
        };

        let err = cfg.validate().expect_err("zero cpus must be rejected");

        assert!(
            err.to_string().contains("cpus"),
            "error should name the offending field, got: {err}"
        );
    }
}
