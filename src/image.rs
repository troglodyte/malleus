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
    #[error(
        "`{path}` is not a disk image (no partition table); \
         it began with: {snippet}. The bad file has been removed — re-run to recreate it"
    )]
    NotADiskImage { path: String, snippet: String },
}

/// Bytes needed to cover both the protective MBR and the GPT header sector.
const HEADER_LEN: usize = 520;

/// How much of a rejected file to quote back in the error.
const SNIPPET_LEN: usize = 120;

/// Report whether `header` opens like a partitioned raw disk image.
///
/// Downloads are the reason this exists: an HTTP error body saved under the
/// image's name is padded to the configured disk size and then boots into
/// nothing, which surfaces much later as a VM that never takes a DHCP lease.
/// Checking for a partition table turns that into an error at download time.
fn looks_like_disk_image(header: &[u8]) -> bool {
    // Protective MBR: boot signature in the last two bytes of the first sector.
    let has_mbr_signature = header.len() >= 512 && header[510] == 0x55 && header[511] == 0xAA;
    // GPT header sits in the second sector, which Debian's cloud images use.
    let has_gpt_header = header.len() >= HEADER_LEN && &header[512..520] == b"EFI PART";

    has_mbr_signature || has_gpt_header
}

/// Render the start of a rejected file as a short, terminal-safe string.
fn describe_header(header: &[u8]) -> String {
    let text: String = String::from_utf8_lossy(header)
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .take(SNIPPET_LEN)
        .collect();

    text.trim().to_string()
}

/// Reject `path` unless it opens like a disk image, deleting it if it does not.
///
/// Removal matters as much as detection: every caller here skips work when the
/// file already exists, so a bad artifact left in place would be reused by every
/// later run instead of being re-fetched.
fn verify_disk_image(path: &Path) -> Result<(), ImageError> {
    let mut header = vec![0u8; HEADER_LEN];
    let mut file = fs::File::open(path)?;
    let read = read_full(&mut file, &mut header)?;
    header.truncate(read);

    if looks_like_disk_image(&header) {
        return Ok(());
    }

    let snippet = describe_header(&header);
    fs::remove_file(path)?;

    Err(ImageError::NotADiskImage {
        path: path.display().to_string(),
        snippet,
    })
}

/// Fill `buf` as far as the file allows, returning how many bytes were read.
fn read_full(file: &mut fs::File, buf: &mut [u8]) -> Result<usize, ImageError> {
    use std::io::Read;

    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
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
        
        // `--fail` is what makes curl's exit status mean anything: without it an
        // HTTP 404 is reported as success and the error body is written to `-o`.
        let status = Command::new("curl")
            .args(["--fail", "-L", "-o", &base_image_path.display().to_string(), &url])
            .status()?;

        if !status.success() {
            // Leave nothing behind that a later run would mistake for a cache hit.
            let _ = fs::remove_file(&base_image_path);
            return Err(ImageError::Download(format!(
                "curl exited with status {status} for {url}"
            )));
        }
    }

    // Also covers a cache poisoned before `--fail` was in place, since a bad file
    // that already exists would otherwise skip the download entirely.
    verify_disk_image(&base_image_path)?;

    let root_disk = state_dir.join("root.raw");
    if !root_disk.exists() {
        println!("Creating root disk from base image...");
        fs::copy(&base_image_path, &root_disk)?;
        
        // Resize to configured size
        let size_bytes = cfg.root_disk_gib * 1024 * 1024 * 1024;
        let file = fs::OpenOptions::new().write(true).open(&root_disk)?;
        file.set_len(size_bytes)?;
    }

    // A root disk cloned from a poisoned cache is itself junk, and it too would
    // be skipped as "already present" on every later run. Growing the partition
    // in-guest rewrites the table but keeps the signature, so a booted VM's disk
    // still passes.
    verify_disk_image(&root_disk)?;

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

#[cfg(test)]
mod tests {
    use super::*;

    /// A real image opens with a protective MBR whose sector ends in 0x55AA.
    fn mbr_header() -> Vec<u8> {
        let mut header = vec![0u8; 512];
        header[510] = 0x55;
        header[511] = 0xAA;
        header
    }

    #[test]
    fn an_html_error_page_is_not_a_disk_image() {
        // `curl` without `--fail` exits 0 on a 404 and writes the body to the
        // output path, so this is exactly what lands under the image's name.
        let body = b"<!DOCTYPE html>\n<html><head><title>404 Not Found</title></head>\n";

        assert!(!looks_like_disk_image(body));
    }

    #[test]
    fn an_empty_or_truncated_download_is_not_a_disk_image() {
        assert!(!looks_like_disk_image(b""));
        assert!(!looks_like_disk_image(&[0u8; 511]));
    }

    #[test]
    fn a_protective_mbr_signature_marks_a_disk_image() {
        assert!(looks_like_disk_image(&mbr_header()));
    }

    #[test]
    fn a_gpt_header_marks_a_disk_image() {
        // Debian's cloud images are GPT-partitioned; the header sits in LBA 1.
        let mut header = vec![0u8; 520];
        header[512..520].copy_from_slice(b"EFI PART");

        assert!(looks_like_disk_image(&header));
    }

    #[test]
    fn a_zeroed_first_sector_without_either_signature_is_rejected() {
        assert!(!looks_like_disk_image(&[0u8; 520]));
    }

    #[test]
    fn the_reported_snippet_stays_printable_and_short() {
        let mut body = b"<!DOCTYPE html>".to_vec();
        body.extend_from_slice(&[0x00, 0xFF]);
        body.extend(std::iter::repeat_n(b'x', 500));

        let snippet = describe_header(&body);

        assert!(snippet.starts_with("<!DOCTYPE html>"));
        assert!(snippet.chars().count() <= SNIPPET_LEN);
        assert!(
            !snippet.contains('\u{0}'),
            "control bytes should not reach the terminal: {snippet:?}"
        );
    }
}
