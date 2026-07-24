use std::{
    fs,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use p256::{
    ecdsa::{DerSignature, SigningKey},
    elliptic_curve::Generate,
    pkcs8::{EncodePrivateKey, LineEnding as Pkcs8LineEnding},
};
use x509_cert::{
    builder::{Builder, CertificateBuilder, profile},
    der::{EncodePem, pem::LineEnding as PemLineEnding},
    name::Name,
    serial_number::SerialNumber,
    spki::SubjectPublicKeyInfo,
    time::Validity,
};

const CLIENT_CERT_FILE: &str = "client.crt";
const CLIENT_KEY_FILE: &str = "client.key";
const SERVER_CERT_FILE: &str = "server.crt";
const SERVER_KEY_FILE: &str = "server.key";
const VALIDITY_DAYS: u64 = 3650;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityMaterial {
    pub cert_pem: String,
    pub key_pem: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkiMaterial {
    pub client: IdentityMaterial,
    pub server: IdentityMaterial,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkiPaths {
    pub client_cert: PathBuf,
    pub client_key: PathBuf,
    pub server_cert: PathBuf,
    pub server_key: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum PkiError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("incomplete PKI material in {dir}")]
    IncompleteMaterial { dir: PathBuf },
    #[error("invalid certificate subject `{subject}`: {details}")]
    InvalidSubject {
        subject: String,
        details: String,
    },
    #[error("certificate generation failed: {0}")]
    CertificateGeneration(String),
}

pub fn pki_paths(base_dir: &Path) -> PkiPaths {
    PkiPaths {
        client_cert: base_dir.join(CLIENT_CERT_FILE),
        client_key: base_dir.join(CLIENT_KEY_FILE),
        server_cert: base_dir.join(SERVER_CERT_FILE),
        server_key: base_dir.join(SERVER_KEY_FILE),
    }
}

fn subject(common_name: &str) -> Result<Name, PkiError> {
    // RFC2253 strings are most-significant RDN first. `Name::from_str` stores
    // them in certificate order, so this text form ends up as C,O,CN.
    let subject = format!("CN={common_name},O=malleus,C=US");
    Name::from_str(&subject).map_err(|err| PkiError::InvalidSubject {
        subject,
        details: err.to_string(),
    })
}

fn generate_identity(common_name: &str, serial: u32) -> Result<IdentityMaterial, PkiError> {
    let signing_key = SigningKey::generate();

    let profile = profile::cabf::Root::new(false, subject(common_name)?)
        .map_err(|err| PkiError::CertificateGeneration(err.to_string()))?;

    let validity = Validity::from_now(Duration::from_secs(60 * 60 * 24 * VALIDITY_DAYS))
        .map_err(|err| PkiError::CertificateGeneration(err.to_string()))?;
    let public_key = SubjectPublicKeyInfo::from_key(signing_key.verifying_key())
        .map_err(|err| PkiError::CertificateGeneration(err.to_string()))?;

    let builder = CertificateBuilder::new(profile, SerialNumber::from(serial), validity, public_key)
        .map_err(|err| PkiError::CertificateGeneration(err.to_string()))?;

    let cert = builder
        .build::<_, DerSignature>(&signing_key)
        .map_err(|err| PkiError::CertificateGeneration(err.to_string()))?;

    let cert_pem = cert
        .to_pem(PemLineEnding::LF)
        .map_err(|err| PkiError::CertificateGeneration(err.to_string()))?;
    let key_pem = signing_key
        .to_pkcs8_pem(Pkcs8LineEnding::LF)
        .map_err(|err| PkiError::CertificateGeneration(err.to_string()))?
        .to_string();

    Ok(IdentityMaterial { cert_pem, key_pem })
}

fn write_identity(cert_path: &Path, key_path: &Path, identity: &IdentityMaterial) -> Result<(), PkiError> {
    fs::write(cert_path, &identity.cert_pem)?;
    fs::write(key_path, &identity.key_pem)?;
    Ok(())
}

fn read_identity(cert_path: &Path, key_path: &Path) -> Result<IdentityMaterial, PkiError> {
    Ok(IdentityMaterial {
        cert_pem: fs::read_to_string(cert_path)?,
        key_pem: fs::read_to_string(key_path)?,
    })
}

fn maybe_load(paths: &PkiPaths) -> Result<Option<PkiMaterial>, PkiError> {
    let existing = [
        paths.client_cert.exists(),
        paths.client_key.exists(),
        paths.server_cert.exists(),
        paths.server_key.exists(),
    ];

    let present_count = existing.iter().filter(|present| **present).count();
    if present_count == 0 {
        return Ok(None);
    }
    if present_count != existing.len() {
        return Err(PkiError::IncompleteMaterial {
            dir: paths
                .client_cert
                .parent()
                .map_or_else(PathBuf::new, Path::to_path_buf),
        });
    }

    let client = read_identity(&paths.client_cert, &paths.client_key)?;
    let server = read_identity(&paths.server_cert, &paths.server_key)?;

    Ok(Some(PkiMaterial { client, server }))
}

/// Load existing PKI material from `base_dir`, or generate and persist it.
pub fn load_or_create(base_dir: &Path) -> Result<PkiMaterial, PkiError> {
    let paths = pki_paths(base_dir);
    if let Some(existing) = maybe_load(&paths)? {
        return Ok(existing);
    }

    fs::create_dir_all(base_dir)?;

    let client = generate_identity("malleus-client", 1)?;
    let server = generate_identity("malleus-server", 2)?;

    write_identity(&paths.client_cert, &paths.client_key, &client)?;
    write_identity(&paths.server_cert, &paths.server_key, &server)?;

    Ok(PkiMaterial { client, server })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use p256::pkcs8::DecodePrivateKey;
    use x509_cert::{Certificate, der::DecodePem};

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

            path.push(format!("malleus-{prefix}-{}-{stamp}-{seq}", std::process::id()));
            fs::create_dir_all(&path).expect("temp test directory should be creatable");

            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn load_or_create_generates_parseable_certificates_and_keys() {
        let temp = TempDir::new("pki-generate");

        let material = load_or_create(&temp.path).expect("pki material should be generated");

        assert!(
            material.client.cert_pem.contains("BEGIN CERTIFICATE"),
            "client cert should be PEM"
        );
        assert!(
            material.server.cert_pem.contains("BEGIN CERTIFICATE"),
            "server cert should be PEM"
        );
        assert!(
            material.client.key_pem.contains("BEGIN PRIVATE KEY"),
            "client key should be PKCS#8 PEM"
        );
        assert!(
            material.server.key_pem.contains("BEGIN PRIVATE KEY"),
            "server key should be PKCS#8 PEM"
        );

        Certificate::from_pem(material.client.cert_pem.as_bytes())
            .expect("client cert PEM should parse");
        Certificate::from_pem(material.server.cert_pem.as_bytes())
            .expect("server cert PEM should parse");

        p256::SecretKey::from_pkcs8_pem(&material.client.key_pem)
            .expect("client key PEM should parse");
        p256::SecretKey::from_pkcs8_pem(&material.server.key_pem)
            .expect("server key PEM should parse");
    }

    #[test]
    fn load_or_create_is_idempotent_and_keeps_existing_material() {
        let temp = TempDir::new("pki-idempotent");

        let first = load_or_create(&temp.path).expect("initial pki generation should succeed");
        let second = load_or_create(&temp.path).expect("existing pki should load");

        assert_eq!(first, second, "existing material should be reused as-is");
        assert_ne!(
            first.client.cert_pem, first.server.cert_pem,
            "client and server certs should be distinct"
        );

        let paths = pki_paths(&temp.path);
        assert!(paths.client_cert.exists());
        assert!(paths.client_key.exists());
        assert!(paths.server_cert.exists());
        assert!(paths.server_key.exists());
    }

    #[test]
    fn load_or_create_rejects_partial_existing_material() {
        let temp = TempDir::new("pki-partial");
        let paths = pki_paths(&temp.path);

        fs::write(&paths.client_cert, "partial").expect("test setup should write partial file");

        let err = load_or_create(&temp.path).expect_err("partial material should be rejected");

        match err {
            PkiError::IncompleteMaterial { dir } => {
                assert_eq!(dir, temp.path);
            }
            other => panic!("expected IncompleteMaterial error, got: {other}"),
        }
    }
}