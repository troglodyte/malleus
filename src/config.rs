use serde::{Deserialize, Serialize};

/// User-editable settings for the malleus VM, read from `~/.malleus/config.toml`.
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
    #[error("memory_mib must be at least 1, got {0}")]
    MemoryMiB(u64),
    #[error("root_disk_gib must be at least 1, got {0}")]
    RootDiskGiB(u64),
    #[error("pool_disk_gib must be at least 1, got {0}")]
    PoolDiskGiB(u64),
}

impl Config {
    /// Reject settings that would fail late and confusingly at boot time.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.cpus == 0 {
            return Err(ConfigError::Cpus(self.cpus));
        }
        if self.memory_mib == 0 {
            return Err(ConfigError::MemoryMiB(self.memory_mib));
        }
        if self.root_disk_gib == 0 {
            return Err(ConfigError::RootDiskGiB(self.root_disk_gib));
        }
        if self.pool_disk_gib == 0 {
            return Err(ConfigError::PoolDiskGiB(self.pool_disk_gib));
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

    #[test]
    fn zero_memory_is_rejected() {
        let cfg = Config {
            memory_mib: 0,
            ..Config::default()
        };

        let err = cfg.validate().expect_err("zero memory must be rejected");

        assert!(
            err.to_string().contains("memory"),
            "error should name the offending field, got: {err}"
        );
    }

    #[test]
    fn zero_root_disk_is_rejected() {
        let cfg = Config {
            root_disk_gib: 0,
            ..Config::default()
        };

        let err = cfg
            .validate()
            .expect_err("zero root disk size must be rejected");

        assert!(
            err.to_string().contains("root_disk"),
            "error should name the offending field, got: {err}"
        );
    }

    #[test]
    fn zero_pool_disk_is_rejected() {
        let cfg = Config {
            pool_disk_gib: 0,
            ..Config::default()
        };

        let err = cfg
            .validate()
            .expect_err("zero pool disk size must be rejected");

        assert!(
            err.to_string().contains("pool_disk"),
            "error should name the offending field, got: {err}"
        );
    }

    #[test]
    fn toml_round_trip_preserves_values() {
        let cfg = Config {
            cpus: 8,
            memory_mib: 8192,
            root_disk_gib: 40,
            pool_disk_gib: 120,
        };

        let toml = toml::to_string(&cfg).expect("config should serialize to TOML");
        let parsed: Config = toml::from_str(&toml).expect("serialized TOML should parse");

        assert_eq!(parsed, cfg);
    }
}
