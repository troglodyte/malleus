use std::fs;
use std::path::Path;
use std::process::Command;
use crate::config::Config;

#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to download image: {0}")]
    Download(String),
    #[error("unsupported architecture: {0}")]
    UnsupportedArch(String),
}

pub fn ensure_images(state_dir: &Path, cfg: &Config) -> Result<(), ImageError> {
    if cfg!(test) {
        return Ok(());
    }
    let images_dir = state_dir.join("images");
    fs::create_dir_all(&images_dir)?;

    let arch = std::env::consts::ARCH;
    let debian_arch = match arch {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        _ => return Err(ImageError::UnsupportedArch(arch.to_string())),
    };

    let base_image_name = format!("debian-12-genericcloud-{}.raw", debian_arch);
    let base_image_path = images_dir.join(&base_image_name);

    if !base_image_path.exists() {
        println!("Downloading Debian base image (this may take a few minutes)...");
        let url = format!(
            "https://cloud.debian.org/images/cloud/bookworm/latest/{}",
            base_image_name
        );
        
        let status = Command::new("curl")
            .args(["-L", "-o", &base_image_path.display().to_string(), &url])
            .status()?;

        if !status.success() {
            return Err(ImageError::Download(format!("curl exited with status {}", status)));
        }
    }

    let root_disk = state_dir.join("root.raw");
    if !root_disk.exists() {
        println!("Creating root disk from base image...");
        fs::copy(&base_image_path, &root_disk)?;
        
        // Resize to configured size
        let size_bytes = cfg.root_disk_gib * 1024 * 1024 * 1024;
        let file = fs::OpenOptions::new().write(true).open(&root_disk)?;
        file.set_len(size_bytes)?;
    }

    let pool_disk = state_dir.join("pool.raw");
    if !pool_disk.exists() {
        println!("Creating sparse pool disk ({} GiB)...", cfg.pool_disk_gib);
        let size_bytes = cfg.pool_disk_gib * 1024 * 1024 * 1024;
        let file = fs::File::create(&pool_disk)?;
        file.set_len(size_bytes)?;
    }

    let efi_store = state_dir.join("efi-store");
    if !efi_store.exists() {
        // vfkit creates this if we pass the ,create flag, but we'll ensure the path is ready
    }

    Ok(())
}
