//! The server's network identity (SPEC R14): its TLS certificate and key,
//! and the credential daemons on other machines present.
//!
//! The identity is created the first time a project's server starts and is
//! never replaced, so the certificate fingerprint that daemons pin stays
//! valid for the project's whole life.

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// SHA-256 of a DER certificate.
pub type Fingerprint = [u8; 32];

/// The certificate's subject name. Daemons pin the certificate by its
/// fingerprint and never check the name, so one name serves every address.
const SUBJECT_NAME: &str = "rosen-server";

pub struct Identity {
    certificate: Vec<u8>,
    private_key: Vec<u8>,
    credential: String,
}

impl Identity {
    /// Reads the project's identity, creating it if this is the first start.
    /// Two servers starting at once agree: only one file is ever written.
    pub fn load_or_create(data_dir: &Path) -> io::Result<Self> {
        let path = identity_path(data_dir);
        match fs::read(&path) {
            Ok(bytes) => return Self::decode(&bytes),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        fs::create_dir_all(path.parent().unwrap_or(data_dir))?;
        let created = Self::generate()?;
        let file = atomicwrites::AtomicFile::new(&path, atomicwrites::DisallowOverwrite);
        match file.write(|out| io::Write::write_all(out, &created.encode())) {
            Ok(()) => {
                restrict_to_owner(&path)?;
                Ok(created)
            }
            // Another server won the race; use its identity.
            Err(atomicwrites::Error::Internal(err))
                if err.kind() == io::ErrorKind::AlreadyExists =>
            {
                Self::decode(&fs::read(&path)?)
            }
            Err(err) => Err(io::Error::other(err.to_string())),
        }
    }

    fn generate() -> io::Result<Self> {
        let certified = rcgen::generate_simple_self_signed(vec![SUBJECT_NAME.to_owned()])
            .map_err(|err| io::Error::other(format!("could not create a certificate: {err}")))?;
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).map_err(|err| io::Error::other(err.to_string()))?;
        Ok(Self {
            certificate: certified.cert.der().to_vec(),
            private_key: certified.signing_key.serialize_der(),
            credential: hex(&secret),
        })
    }

    fn encode(&self) -> Vec<u8> {
        let json = serde_json::json!({
            "certificate": STANDARD.encode(&self.certificate),
            "private_key": STANDARD.encode(&self.private_key),
            "credential": self.credential,
        });
        serde_json::to_vec_pretty(&json).expect("a JSON value always serialises")
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        let invalid = |what: &str| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("network identity file: bad or missing {what}"),
            )
        };
        let json: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| invalid("JSON"))?;
        let field = |name: &str| json[name].as_str().ok_or_else(|| invalid(name));
        let base64 = |name: &str| STANDARD.decode(field(name)?).map_err(|_| invalid(name));
        Ok(Self {
            certificate: base64("certificate")?,
            private_key: base64("private_key")?,
            credential: field("credential")?.to_owned(),
        })
    }

    pub fn certificate(&self) -> &[u8] {
        &self.certificate
    }

    pub fn private_key(&self) -> &[u8] {
        &self.private_key
    }

    /// The credential daemons on other machines present in their hello.
    pub fn credential(&self) -> &str {
        &self.credential
    }

    pub fn fingerprint(&self) -> Fingerprint {
        fingerprint_of(&self.certificate)
    }

    /// Compares digests rather than the strings, so the time taken does not
    /// reveal how much of a guessed credential was right.
    pub fn credential_matches(&self, presented: &str) -> bool {
        let expected = Sha256::digest(self.credential.as_bytes());
        let given = Sha256::digest(presented.as_bytes());
        expected
            .iter()
            .zip(given.iter())
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0
    }
}

/// Where the identity lives in a server's data directory.
pub fn identity_path(data_dir: &Path) -> PathBuf {
    data_dir.join("network").join("identity.json")
}

pub fn fingerprint_of(certificate_der: &[u8]) -> Fingerprint {
    Sha256::digest(certificate_der).into()
}

/// Lowercase hex, as `rosen server status` prints it.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Parses a fingerprint in hex, with or without `:` separators.
pub fn parse_fingerprint(text: &str) -> Result<Fingerprint, String> {
    let digits: String = text.chars().filter(|c| *c != ':').collect();
    let bad = || format!("{text:?} is not a SHA-256 fingerprint (64 hex digits)");
    if digits.len() != 64 || !digits.is_ascii() {
        return Err(bad());
    }
    let mut out = [0u8; 32];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&digits[index * 2..index * 2 + 2], 16).map_err(|_| bad())?;
    }
    Ok(out)
}

/// The file holds the TLS private key and the credential. On Unix it is
/// made readable by its owner only; on Windows it inherits the data
/// directory's access list, which a user-profile directory already limits.
#[cfg(unix)]
fn restrict_to_owner(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_to_owner(_path: &Path) -> io::Result<()> {
    Ok(())
}
