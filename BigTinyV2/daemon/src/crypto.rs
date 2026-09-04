//! At-rest encryption for secrets BigTiny persists in its own SQLite DB
//! (provider API keys, MCP server auth headers) — those used to be written
//! as plain JSON text.
//!
//! Key resolution: `BIGTINY_ENCRYPTION_KEY` (a stable, hex-encoded 32-byte
//! key Kitty generates once and stores in Windows Credential Manager,
//! passed via env on every launch — see `src-tauri/src/lifecycle/
//! bigtiny_proc.rs::spawn` and `config/providers/keyring.rs`) is the primary
//! source. When BigTiny runs standalone (no Kitty parent process), that env
//! var is absent — falls back to a key file (`{data_dir}/encryption.key`)
//! this module generates once and persists itself. Either way, `init` must
//! run before anything else in `lib.rs::run()` that might decrypt a stored
//! value (`ProviderRouter::load_providers`, `MCPManager::connect_all`).
//!
//! `encrypt`/`decrypt` are exposed as free functions reading a
//! process-global key (a `OnceCell`, not threaded through every call site)
//! because the functions that need to decrypt — `provider::router::
//! register_from_row`, `mcp::manager::row_to_config` — are plain,
//! `AppState`-less functions called from several places, including at
//! startup before any `AppState` exists. Threading a key parameter through
//! every one of those signatures would be a large, purely mechanical change
//! for no real benefit over a single process-wide key.

use std::path::Path;

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Key};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use once_cell::sync::OnceCell;

use crate::error::DaemonError;

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const PREFIX: &str = "enc:v1:";
const KEY_FILE_NAME: &str = "encryption.key";

static CIPHER: OnceCell<Aes256Gcm> = OnceCell::new();

/// Resolve the encryption key (env var, then key file) and initialize the
/// process-global cipher. Must be called before `lib.rs::run()` does
/// anything that might decrypt a stored value — see the module doc comment.
/// A failure here (malformed env value, or a key file that can't be read or
/// written) is a hard startup error rather than a silent fallback to
/// running unencrypted.
pub fn init(data_dir: &Path, env_key: Option<&str>) -> Result<(), DaemonError> {
    let key_bytes = match env_key {
        Some(hex) => decode_hex_key(hex)
            .map_err(|e| DaemonError::Crypto(format!("BIGTINY_ENCRYPTION_KEY: {e}")))?,
        None => load_or_create_key_file(data_dir)?,
    };
    let key = Key::<Aes256Gcm>::from_slice(&key_bytes);
    let cipher = Aes256Gcm::new(key);
    CIPHER
        .set(cipher)
        .map_err(|_| DaemonError::Crypto("crypto::init called more than once".to_string()))?;
    Ok(())
}

/// The process-global cipher, initialized via `init` on the real daemon's
/// startup path. Code that constructs its own `AppState` without ever
/// calling `run()`/`init` (every test in this crate) instead gets a
/// lazily-generated, in-memory-only random key the first time `encrypt`/
/// `decrypt` is actually used — fine for those callers since round-trip
/// correctness within one process is all they need, not a specific known
/// key or persistence across runs.
fn cipher() -> &'static Aes256Gcm {
    CIPHER.get_or_init(|| {
        tracing::warn!(
            "crypto::init was never called — using an ephemeral in-memory key \
             (expected in tests; a bug if seen from the real daemon binary)"
        );
        Aes256Gcm::new(&Aes256Gcm::generate_key(&mut OsRng))
    })
}

fn decode_hex_key(hex: &str) -> Result<[u8; KEY_LEN], String> {
    let bytes = hex_decode(hex.trim())?;
    bytes
        .try_into()
        .map_err(|v: Vec<u8>| format!("expected {KEY_LEN} bytes (64 hex chars), got {}", v.len()))
}

fn load_or_create_key_file(data_dir: &Path) -> Result<[u8; KEY_LEN], DaemonError> {
    let path = data_dir.join(KEY_FILE_NAME);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        return decode_hex_key(&existing)
            .map_err(|e| DaemonError::Crypto(format!("{}: {e}", path.display())));
    }
    std::fs::create_dir_all(data_dir)?;
    let mut key_bytes = [0u8; KEY_LEN];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut key_bytes);
    std::fs::write(&path, hex_encode(&key_bytes))?;
    tracing::info!(
        "generated a new at-rest encryption key at {} (no BIGTINY_ENCRYPTION_KEY set — standalone mode)",
        path.display()
    );
    Ok(key_bytes)
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err("odd-length hex string".to_string());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

/// Encrypt `plaintext`, returning `"enc:v1:" + base64(nonce || ciphertext)`.
/// A fresh random nonce every call — AES-GCM security depends on never
/// reusing a (key, nonce) pair.
pub fn encrypt(plaintext: &str) -> String {
    let cipher = cipher();
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(&nonce, plaintext.as_bytes())
        .expect("AES-GCM encryption cannot fail for a well-formed key/nonce");
    let mut payload = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    payload.extend_from_slice(&nonce);
    payload.extend_from_slice(&ciphertext);
    format!("{PREFIX}{}", BASE64.encode(payload))
}

/// Decrypt a value previously produced by `encrypt`. A value with no
/// `"enc:v1:"` prefix is treated as legacy plaintext (pre-encryption rows)
/// and returned unchanged — this is what lets an existing, never-re-saved
/// row keep working with no migration pass: the next write that touches it
/// opportunistically re-encrypts it via `encrypt`. A prefixed value that
/// fails to decrypt (wrong key, corrupted data) is logged and also returned
/// unchanged rather than panicking — this must stay infallible from the
/// caller's perspective, since `register_from_row`/`row_to_config` have no
/// error path of their own to report through.
pub fn decrypt(value: &str) -> String {
    let Some(encoded) = value.strip_prefix(PREFIX) else {
        return value.to_string();
    };
    let cipher = cipher();
    let payload = match BASE64.decode(encoded) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("failed to base64-decode an encrypted value: {e}");
            return value.to_string();
        }
    };
    if payload.len() < NONCE_LEN {
        tracing::warn!("encrypted value too short to contain a nonce");
        return value.to_string();
    }
    let (nonce_bytes, ciphertext) = payload.split_at(NONCE_LEN);
    let nonce = aes_gcm::Nonce::from_slice(nonce_bytes);
    match cipher.decrypt(nonce, ciphertext) {
        Ok(plaintext) => String::from_utf8(plaintext).unwrap_or_else(|e| {
            tracing::warn!("decrypted value was not valid UTF-8: {e}");
            value.to_string()
        }),
        Err(e) => {
            tracing::warn!("failed to decrypt a stored value: {e}");
            value.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_test_cipher() {
        // Each test file/thread shares the process-global CIPHER OnceCell —
        // `set` is a no-op (returns Err, ignored) if another test already
        // initialized it first, which is fine: all tests in this module use
        // the same arbitrary-but-fixed key, so results are still correct
        // and deterministic regardless of run order.
        let key = [7u8; KEY_LEN];
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
        let _ = CIPHER.set(cipher);
    }

    #[test]
    fn round_trips_plain_ascii() {
        init_test_cipher();
        let original = "sk-abc123";
        assert_eq!(decrypt(&encrypt(original)), original);
    }

    #[test]
    fn round_trips_empty_string() {
        init_test_cipher();
        assert_eq!(decrypt(&encrypt("")), "");
    }

    #[test]
    fn round_trips_unicode() {
        init_test_cipher();
        let original = "héllo — wörld 🔑";
        assert_eq!(decrypt(&encrypt(original)), original);
    }

    #[test]
    fn encrypting_the_same_plaintext_twice_produces_different_ciphertext() {
        init_test_cipher();
        let a = encrypt("same input");
        let b = encrypt("same input");
        assert_ne!(a, b, "nonce reuse would make these identical");
        // Both still decrypt back to the same original.
        assert_eq!(decrypt(&a), "same input");
        assert_eq!(decrypt(&b), "same input");
    }

    #[test]
    fn a_value_with_no_prefix_passes_through_unchanged_as_legacy_plaintext() {
        init_test_cipher();
        assert_eq!(
            decrypt("sk-legacy-plaintext-key"),
            "sk-legacy-plaintext-key"
        );
    }

    #[test]
    fn corrupted_ciphertext_does_not_panic_and_returns_the_input() {
        init_test_cipher();
        let garbage = format!("{PREFIX}not-valid-base64!!!");
        assert_eq!(decrypt(&garbage), garbage);
    }

    #[test]
    fn truncated_payload_too_short_for_a_nonce_does_not_panic() {
        init_test_cipher();
        let too_short = format!("{PREFIX}{}", BASE64.encode(b"x"));
        assert_eq!(decrypt(&too_short), too_short);
    }

    #[test]
    fn hex_round_trips() {
        let bytes = [1u8, 2, 255, 0, 128];
        assert_eq!(hex_decode(&hex_encode(&bytes)).unwrap(), bytes);
    }

    #[test]
    fn decode_hex_key_rejects_wrong_length() {
        assert!(decode_hex_key("abcd").is_err());
    }
}
