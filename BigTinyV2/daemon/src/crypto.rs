//! At-rest encryption for secrets BigTiny persists in its own SQLite DB
//! (provider API keys, MCP server auth headers) — those used to be written
//! as plain JSON text.
//!
//! Key resolution: `BIGTINY_ENCRYPTION_KEY` (a stable, hex-encoded 32-byte
//! key Kitty generates once and stores in Windows Credential Manager,
//! passed via env on every launch — see `src-tauri/src/lifecycle/
//! bigtiny_proc.rs::spawn` and `config/providers/keyring.rs`) is the primary
//! source. When BigTiny runs standalone (no Kitty parent process), that env
//! var is absent — falls back to a key file this module generates once and
//! persists itself (see [`key_file_path`]). Either way, `init` must
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
//!
//! **The key file is not plaintext on Windows.** It is sealed with DPAPI
//! (user scope) as `encryption.key.dpapi`, so a copy of the data directory
//! taken off the machine, or read by another user, does not carry a usable
//! key alongside the ciphertext it protects. A plaintext `encryption.key`
//! left by an earlier version is sealed and then removed the first time it
//! is read. Other platforms keep the hex file: Android supplies its key by
//! env from the platform keystore, and nothing else ships there.

use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Key};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use once_cell::sync::OnceCell;

use crate::error::DaemonError;

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const PREFIX: &str = "enc:v1:";
/// The plaintext hex key file: the stored form off Windows, and what an
/// earlier version left on Windows.
const KEY_FILE_NAME: &str = "encryption.key";
/// The DPAPI-sealed key file (Windows only).
#[cfg(windows)]
const SEALED_KEY_FILE_NAME: &str = "encryption.key.dpapi";

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

/// Write `hex` as the daemon's at-rest key, unless one is already stored.
///
/// Used by the V1 import and nothing else. `init` with an env key configures
/// only the *current process*, which is enough to verify that imported rows
/// decrypt but not to keep them readable afterwards: the next daemon start
/// without that env var would generate a fresh key and every imported
/// provider row would fail to decrypt. Since the rows are not re-encrypted by
/// the import, continuity requires the daemon to adopt V1's key permanently —
/// which is what "carry the encryption key across" actually means.
///
/// Refuses to overwrite an existing key file: doing so would render anything
/// *already* encrypted under it unreadable, which is a worse outcome than a
/// failed import.
pub fn adopt_key(data_dir: &Path, hex: &str) -> Result<bool, DaemonError> {
    let key_bytes = decode_hex_key(hex).map_err(DaemonError::Crypto)?;
    if read_stored_key(data_dir)?.is_some() {
        return Ok(false);
    }
    write_stored_key(data_dir, &key_bytes)?;
    Ok(true)
}

/// Where this daemon keeps its at-rest key under `data_dir`: the
/// DPAPI-sealed file on Windows, the hex file elsewhere.
pub fn key_file_path(data_dir: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        data_dir.join(SEALED_KEY_FILE_NAME)
    }
    #[cfg(not(windows))]
    {
        data_dir.join(KEY_FILE_NAME)
    }
}

fn load_or_create_key_file(data_dir: &Path) -> Result<[u8; KEY_LEN], DaemonError> {
    if let Some(key) = read_stored_key(data_dir)? {
        return Ok(key);
    }
    let mut key_bytes = [0u8; KEY_LEN];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut key_bytes);
    write_stored_key(data_dir, &key_bytes)?;
    tracing::info!(
        "generated a new at-rest encryption key at {} (no BIGTINY_ENCRYPTION_KEY set — standalone mode)",
        key_file_path(data_dir).display()
    );
    Ok(key_bytes)
}

/// The key stored under `data_dir`, if there is one.
///
/// On Windows this also finishes the move off plaintext: a hex
/// `encryption.key` with no sealed file beside it is sealed, the sealed copy
/// is read back and checked, and only then is the plaintext file removed. A
/// crash at any point leaves at least one readable copy. A plaintext file
/// found *beside* a sealed one (a crash after sealing) is removed if it holds
/// the same key and left alone, with a warning, if it does not.
fn read_stored_key(data_dir: &Path) -> Result<Option<[u8; KEY_LEN]>, DaemonError> {
    let plain_path = data_dir.join(KEY_FILE_NAME);
    let read_plain = || -> Result<Option<[u8; KEY_LEN]>, DaemonError> {
        match std::fs::read_to_string(&plain_path) {
            Ok(text) => decode_hex_key(&text)
                .map(Some)
                .map_err(|e| DaemonError::Crypto(format!("{}: {e}", plain_path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    };

    #[cfg(not(windows))]
    {
        read_plain()
    }

    #[cfg(windows)]
    {
        let sealed_path = data_dir.join(SEALED_KEY_FILE_NAME);
        let sealed = match std::fs::read(&sealed_path) {
            Ok(blob) => Some(unseal_key(&blob, &sealed_path)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let plain = read_plain()?;
        match (sealed, plain) {
            (Some(key), None) => Ok(Some(key)),
            (Some(key), Some(leftover)) => {
                if leftover == key {
                    remove_plaintext(&plain_path);
                } else {
                    tracing::warn!(
                        "{} holds a different key from {}; using the sealed one and leaving the \
                         plaintext file in place",
                        plain_path.display(),
                        sealed_path.display()
                    );
                }
                Ok(Some(key))
            }
            (None, Some(key)) => {
                write_stored_key(data_dir, &key)?;
                let read_back = unseal_key(&std::fs::read(&sealed_path)?, &sealed_path)?;
                if read_back != key {
                    return Err(DaemonError::Crypto(format!(
                        "sealing {} did not round-trip; the plaintext key was left in place",
                        plain_path.display()
                    )));
                }
                remove_plaintext(&plain_path);
                tracing::info!(
                    "moved the at-rest encryption key from {} to DPAPI-sealed {}",
                    plain_path.display(),
                    sealed_path.display()
                );
                Ok(Some(key))
            }
            (None, None) => Ok(None),
        }
    }
}

/// Persist `key` as this platform's stored form, atomically: written beside
/// the destination and renamed over it, so a crash never leaves a truncated
/// key file (which would read as a malformed key and stop the daemon).
fn write_stored_key(data_dir: &Path, key: &[u8; KEY_LEN]) -> Result<(), DaemonError> {
    std::fs::create_dir_all(data_dir)?;
    #[cfg(windows)]
    let contents = dpapi::protect(key)
        .map_err(|e| DaemonError::Crypto(format!("could not seal the encryption key: {e}")))?;
    #[cfg(not(windows))]
    let contents = hex_encode(key).into_bytes();

    let dest = key_file_path(data_dir);
    let tmp = dest.with_extension("tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, &dest)?;
    Ok(())
}

#[cfg(windows)]
fn unseal_key(blob: &[u8], path: &Path) -> Result<[u8; KEY_LEN], DaemonError> {
    let bytes = dpapi::unprotect(blob).map_err(|e| {
        DaemonError::Crypto(format!(
            "{}: could not unseal the encryption key (was the data directory copied from \
             another user or machine?): {e}",
            path.display()
        ))
    })?;
    bytes.try_into().map_err(|v: Vec<u8>| {
        DaemonError::Crypto(format!(
            "{}: expected {KEY_LEN} bytes, got {}",
            path.display(),
            v.len()
        ))
    })
}

#[cfg(windows)]
fn remove_plaintext(path: &Path) {
    if let Err(e) = std::fs::remove_file(path) {
        tracing::warn!(
            "could not remove the plaintext key file {}: {e}",
            path.display()
        );
    }
}

/// Windows DPAPI, user scope: only this Windows user on this machine can
/// unseal what it seals.
#[cfg(windows)]
mod dpapi {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    pub fn protect(data: &[u8]) -> std::io::Result<Vec<u8>> {
        let input = blob_of(data)?;
        let mut output = CRYPT_INTEGER_BLOB::default();
        // SAFETY: `input` points at `data`, which outlives the call; every
        // optional pointer is null, which the API accepts; `output` is
        // written by the call and freed below with `LocalFree` as documented.
        let ok = unsafe {
            CryptProtectData(
                &input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(take(output, false))
    }

    pub fn unprotect(data: &[u8]) -> std::io::Result<Vec<u8>> {
        let input = blob_of(data)?;
        let mut output = CRYPT_INTEGER_BLOB::default();
        // SAFETY: as in `protect`; the description out-pointer is null, so
        // there is no second allocation to free.
        let ok = unsafe {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(take(output, true))
    }

    fn blob_of(data: &[u8]) -> std::io::Result<CRYPT_INTEGER_BLOB> {
        let len = u32::try_from(data.len())
            .map_err(|_| std::io::Error::other("input too large for DPAPI"))?;
        Ok(CRYPT_INTEGER_BLOB {
            cbData: len,
            // DPAPI takes a mutable pointer but does not write through the
            // input blob.
            pbData: data.as_ptr().cast_mut(),
        })
    }

    /// Copy out and free a DPAPI output buffer, wiping it first when it held
    /// plaintext.
    fn take(output: CRYPT_INTEGER_BLOB, wipe: bool) -> Vec<u8> {
        if output.pbData.is_null() {
            return Vec::new();
        }
        let len = output.cbData as usize;
        // SAFETY: on success DPAPI hands back `cbData` initialized bytes at
        // `pbData`, allocated with `LocalAlloc`, which we own until freed.
        unsafe {
            let bytes = std::slice::from_raw_parts(output.pbData, len).to_vec();
            if wipe {
                std::ptr::write_bytes(output.pbData, 0, len);
            }
            LocalFree(output.pbData.cast());
            bytes
        }
    }
}

// The stored form off Windows; on Windows only the tests need it.
#[cfg(any(not(windows), test))]
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
    seal(cipher(), plaintext)
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
    match open(cipher(), encoded) {
        Ok(plaintext) => plaintext,
        Err(e) => {
            tracing::warn!("{e}");
            value.to_string()
        }
    }
}

/// A hex-encoded 32-byte key, as `BIGTINY_ENCRYPTION_KEY` carries one.
pub fn parse_key_hex(hex: &str) -> Result<[u8; KEY_LEN], String> {
    decode_hex_key(hex)
}

/// Decrypt `value` with an explicit key rather than this daemon's own - for
/// reading another database's secrets (the V1 import). Legacy plaintext comes
/// back as it is; `None` means a prefixed value this key cannot open.
pub fn decrypt_with_key(value: &str, key: &[u8; KEY_LEN]) -> Option<String> {
    let Some(encoded) = value.strip_prefix(PREFIX) else {
        return Some(value.to_string());
    };
    open(&Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key)), encoded).ok()
}

/// `encrypt` under an explicit key; the test fixture for `decrypt_with_key`.
#[cfg(test)]
pub(crate) fn encrypt_with_key(plaintext: &str, key: &[u8; KEY_LEN]) -> String {
    seal(
        &Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key)),
        plaintext,
    )
}

fn seal(cipher: &Aes256Gcm, plaintext: &str) -> String {
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(&nonce, plaintext.as_bytes())
        .expect("AES-GCM encryption cannot fail for a well-formed key/nonce");
    let mut payload = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    payload.extend_from_slice(&nonce);
    payload.extend_from_slice(&ciphertext);
    format!("{PREFIX}{}", BASE64.encode(payload))
}

/// Open the base64 payload after the prefix.
fn open(cipher: &Aes256Gcm, encoded: &str) -> Result<String, String> {
    let payload = BASE64
        .decode(encoded)
        .map_err(|e| format!("failed to base64-decode an encrypted value: {e}"))?;
    if payload.len() < NONCE_LEN {
        return Err("encrypted value too short to contain a nonce".to_string());
    }
    let (nonce_bytes, ciphertext) = payload.split_at(NONCE_LEN);
    let nonce = aes_gcm::Nonce::from_slice(nonce_bytes);
    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| format!("failed to decrypt a stored value: {e}"))?;
    String::from_utf8(plaintext).map_err(|e| format!("decrypted value was not valid UTF-8: {e}"))
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

    fn temp_data_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bt-crypto-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A generated key is stored, and the same key comes back on the next
    /// start.
    #[test]
    fn a_generated_key_is_stored_and_reloaded() {
        let dir = temp_data_dir("gen");
        let first = load_or_create_key_file(&dir).unwrap();
        assert!(key_file_path(&dir).is_file());
        assert_eq!(load_or_create_key_file(&dir).unwrap(), first);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Windows: the stored key is never plaintext, and a plaintext key left by
    /// an earlier version is sealed and removed on first read.
    #[cfg(windows)]
    #[test]
    fn a_plaintext_key_is_sealed_then_removed() {
        let dir = temp_data_dir("migrate");
        let key = [9u8; KEY_LEN];
        std::fs::write(dir.join(KEY_FILE_NAME), hex_encode(&key)).unwrap();

        assert_eq!(load_or_create_key_file(&dir).unwrap(), key);
        assert!(!dir.join(KEY_FILE_NAME).exists(), "plaintext removed");
        let sealed = std::fs::read(dir.join(SEALED_KEY_FILE_NAME)).unwrap();
        assert!(
            !sealed.windows(KEY_LEN).any(|w| w == key)
                && !String::from_utf8_lossy(&sealed).contains(&hex_encode(&key)),
            "the sealed file must not contain the key in the clear"
        );
        assert_eq!(
            load_or_create_key_file(&dir).unwrap(),
            key,
            "still the same key"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `adopt_key` never replaces a stored key, sealed or not.
    #[test]
    fn adopting_a_key_never_overwrites_a_stored_one() {
        let dir = temp_data_dir("adopt");
        let hex = hex_encode(&[3u8; KEY_LEN]);
        assert!(adopt_key(&dir, &hex).unwrap());
        assert_eq!(load_or_create_key_file(&dir).unwrap(), [3u8; KEY_LEN]);
        assert!(!adopt_key(&dir, &hex_encode(&[4u8; KEY_LEN])).unwrap());
        assert_eq!(load_or_create_key_file(&dir).unwrap(), [3u8; KEY_LEN]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_explicit_key_opens_only_its_own_values() {
        let key = [5u8; KEY_LEN];
        let sealed = encrypt_with_key("sk-v1", &key);
        assert_eq!(decrypt_with_key(&sealed, &key).as_deref(), Some("sk-v1"));
        assert_eq!(decrypt_with_key(&sealed, &[6u8; KEY_LEN]), None);
        assert_eq!(decrypt_with_key("plain", &key).as_deref(), Some("plain"));
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
