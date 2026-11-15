//! A machine's receipt signing key (SPEC.md §2 "Receipt", R8 step 1, R14).
//!
//! Every machine's daemon holds one Ed25519 key, created on first use in its
//! data directory and never replaced. It lives here rather than in the
//! server so `rosen machine join` can read the same key the daemon will sign
//! with and send its public half at enrollment.

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};
use rosen_platform::owner_only;
use rosen_protocol::ReceiptSignature;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub struct MachineKey {
    pair: Ed25519KeyPair,
}

impl MachineKey {
    /// Reads the machine's signing key, creating it on first use. Two
    /// daemons starting at once agree: only one file is ever written.
    pub fn load_or_create(data_dir: &Path) -> io::Result<Self> {
        let path = key_path(data_dir);
        match fs::read(&path) {
            Ok(bytes) => return Self::decode(&bytes),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        fs::create_dir_all(path.parent().unwrap_or(data_dir))?;
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
            .map_err(|_| io::Error::other("could not create a receipt signing key"))?;
        let encoded = STANDARD.encode(pkcs8.as_ref()).into_bytes();
        // The private key proves this machine ran the checks, so only this OS
        // user may open it, as for the server's TLS key.
        match owner_only::write(&path, &encoded, owner_only::Replace::Refuse) {
            Ok(()) => Self::decode(&encoded),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                Self::decode(&fs::read(&path)?)
            }
            Err(err) => Err(err),
        }
    }

    /// A key that lives only in memory, for tests and tools.
    pub fn generate() -> Self {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
            .expect("the system random source is available");
        Self::from_pkcs8(pkcs8.as_ref()).expect("a fresh key parses")
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "receipt signing key file is damaged",
            )
        };
        let text = std::str::from_utf8(bytes).map_err(|_| invalid())?;
        let pkcs8 = STANDARD.decode(text.trim()).map_err(|_| invalid())?;
        Self::from_pkcs8(&pkcs8).ok_or_else(invalid)
    }

    fn from_pkcs8(pkcs8: &[u8]) -> Option<Self> {
        Ed25519KeyPair::from_pkcs8(pkcs8)
            .ok()
            .map(|pair| Self { pair })
    }

    /// The public key in lowercase hex, as the server stores it.
    pub fn public_key(&self) -> String {
        hex(self.pair.public_key().as_ref())
    }

    pub fn sign(&self, payload: &[u8]) -> ReceiptSignature {
        ReceiptSignature {
            public_key: self.public_key(),
            signature: hex(self.pair.sign(payload).as_ref()),
        }
    }
}

/// Where a daemon keeps its signing key: beside the network identity.
pub fn key_path(data_dir: &Path) -> PathBuf {
    data_dir.join("network").join("receipt-signing-key")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
