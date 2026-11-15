//! Machine signing keys for receipts (SPEC.md §2 "Receipt", R8 step 1, R14).
//!
//! Every machine's daemon holds one Ed25519 key, created on first use and
//! never replaced. It signs each receipt a client on that machine submits.
//! The joining daemon sends its public key with its enrollment request and
//! the server binds it to the new machine in the enrollment transaction, so
//! a stolen credential never gets to bind a key of its own. A machine
//! enrolled without one (before keys were sent) is bound the first time the
//! server accepts a signed receipt over its credential. Either way the
//! server refuses any other key for it afterwards. The server's own machine
//! signs with its own key, registered under [`SERVER_MACHINE`].

use ring::signature::{UnparsedPublicKey, ED25519};

/// The key file is read by the daemon and by `rosen machine join`, which
/// sends its public half at enrollment, so the client crate holds it.
pub use rosen_client::receipt_key::{key_path, MachineKey};

/// The machine id the server's own machine signs under. Enrolled machine
/// ids are 43-character random strings, so this one never names one.
pub const SERVER_MACHINE: &str = "server";

/// True when `signature` (hex) is `public_key`'s (hex) Ed25519 signature
/// over `payload`. Malformed hex never verifies.
pub fn verify(public_key: &str, payload: &[u8], signature: &str) -> bool {
    let (Some(key), Some(signature)) = (unhex(public_key), unhex(signature)) else {
        return false;
    };
    UnparsedPublicKey::new(&ED25519, key)
        .verify(payload, &signature)
        .is_ok()
}

/// True when `text` is an Ed25519 public key as [`MachineKey::public_key`]
/// writes it: 32 bytes in lowercase hex.
pub fn is_public_key(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).ok())
        .collect()
}
