//! Minimal GGUF header reader for the Settings model card (§6.3).
//!
//! Deliberately partial. A full GGUF parser would read every tensor
//! descriptor; the card shows four fields, so this reads the metadata
//! key-value block and stops. Anything unreadable comes back `None` — a model
//! card with blanks is fine, a failed download because we couldn't parse an
//! optional field is not.
//!
//! Format: `"GGUF"` magic, u32 version, u64 tensor count, u64 kv count, then
//! `kv_count` entries of `<u64 len><utf8 key><u32 type><value>`. Little-endian
//! throughout.

use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

const MAGIC: &[u8; 4] = b"GGUF";
/// Stop after this many KV pairs. Real files have tens; a corrupt header can
/// claim billions, and we'd sit there reading garbage.
const MAX_KV: u64 = 4096;
/// Longest plausible key or string value. Guards the same corruption case.
const MAX_STR: u64 = 64 * 1024;

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct GgufInfo {
    /// e.g. `"lfm2"`, `"qwen3"` — `general.architecture`.
    pub architecture: Option<String>,
    /// Training context length, `<arch>.context_length`.
    pub context_length: Option<u64>,
    /// Embedding width, `<arch>.embedding_length`. Present on embedders.
    pub embedding_length: Option<u64>,
    /// `general.quantization_version` is a format version, not the quant
    /// scheme, so this is derived from the filename instead (`Q4_K_M`).
    pub quantization: Option<String>,
}

/// Guess the quantisation from a filename. GGUF has no metadata key for it —
/// the community convention is the filename suffix, and every published model
/// follows it.
pub fn quantization_from_name(file_stem: &str) -> Option<String> {
    let upper = file_stem.to_ascii_uppercase();
    for part in upper.rsplit(['-', '.', '_']) {
        if part.starts_with('Q') && part.len() >= 2 && part[1..2].chars().all(|c| c.is_ascii_digit())
        {
            // Recover the full suffix (`Q4_K_M`), not just the last token.
            if let Some(idx) = upper.find(part) {
                return Some(upper[idx..].trim_matches(['-', '.']).to_string());
            }
        }
    }
    if upper.contains("F16") {
        return Some("F16".into());
    }
    if upper.contains("BF16") {
        return Some("BF16".into());
    }
    None
}

fn read_u32(r: &mut impl Read) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64(r: &mut impl Read) -> std::io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

fn read_str(r: &mut impl Read) -> std::io::Result<String> {
    let len = read_u64(r)?;
    if len > MAX_STR {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "implausible string length",
        ));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Skip a KV value of GGUF type `ty`, returning its scalar value when it's
/// one of the integer types we care about.
fn read_value(r: &mut (impl Read + Seek), ty: u32) -> std::io::Result<Option<u64>> {
    Ok(match ty {
        0 | 1 => {
            // uint8 / int8
            let mut b = [0u8; 1];
            r.read_exact(&mut b)?;
            Some(b[0] as u64)
        }
        2 | 3 => {
            // uint16 / int16
            let mut b = [0u8; 2];
            r.read_exact(&mut b)?;
            Some(u16::from_le_bytes(b) as u64)
        }
        4 | 5 => Some(read_u32(r)? as u64), // uint32 / int32
        6 => {
            r.seek(SeekFrom::Current(4))?; // float32
            None
        }
        7 => {
            r.seek(SeekFrom::Current(1))?; // bool
            None
        }
        8 => {
            read_str(r)?;
            None
        }
        9 => {
            // array: <u32 elem type><u64 count><elements>
            let elem = read_u32(r)?;
            let count = read_u64(r)?;
            for _ in 0..count.min(MAX_STR) {
                read_value(r, elem)?;
            }
            None
        }
        10 | 11 => Some(read_u64(r)?), // uint64 / int64
        12 => {
            r.seek(SeekFrom::Current(8))?; // float64
            None
        }
        other => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown gguf value type {other}"),
            ))
        }
    })
}

/// Read what the model card needs. `None` if `path` isn't a readable GGUF.
pub fn read_info(path: &Path) -> Option<GgufInfo> {
    let f = std::fs::File::open(path).ok()?;
    let mut r = BufReader::new(f);

    let mut magic = [0u8; 4];
    r.read_exact(&mut magic).ok()?;
    if &magic != MAGIC {
        return None;
    }
    let _version = read_u32(&mut r).ok()?;
    let _tensor_count = read_u64(&mut r).ok()?;
    let kv_count = read_u64(&mut r).ok()?;

    let mut info = GgufInfo {
        quantization: path
            .file_stem()
            .and_then(|s| quantization_from_name(&s.to_string_lossy())),
        ..Default::default()
    };

    for _ in 0..kv_count.min(MAX_KV) {
        let key = read_str(&mut r).ok()?;
        let ty = read_u32(&mut r).ok()?;
        // The architecture-prefixed keys can't be matched until we've seen
        // `general.architecture`, which GGUF always writes first.
        let want_ctx = info
            .architecture
            .as_ref()
            .map(|a| key == format!("{a}.context_length"))
            .unwrap_or(false);
        let want_embd = info
            .architecture
            .as_ref()
            .map(|a| key == format!("{a}.embedding_length"))
            .unwrap_or(false);

        if key == "general.architecture" && ty == 8 {
            info.architecture = Some(read_str(&mut r).ok()?);
            continue;
        }
        let value = read_value(&mut r, ty).ok()?;
        if want_ctx {
            info.context_length = value;
        } else if want_embd {
            info.embedding_length = value;
        }
    }
    Some(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantization_comes_from_the_filename_convention() {
        assert_eq!(
            quantization_from_name("LFM2.5-1.2B-Instruct-Q4_K_M").as_deref(),
            Some("Q4_K_M")
        );
        assert_eq!(
            quantization_from_name("Qwen3-Embedding-0.6B-q4_k_m").as_deref(),
            Some("Q4_K_M")
        );
        assert_eq!(quantization_from_name("model-f16").as_deref(), Some("F16"));
        assert_eq!(quantization_from_name("mystery-model"), None);
    }

    /// A non-GGUF (or truncated) file must read as "no info", never panic —
    /// this runs over whatever happens to be in the models directory.
    #[test]
    fn a_non_gguf_file_yields_no_info() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("not-a-model.gguf");
        std::fs::write(&p, b"this is not a gguf").unwrap();
        assert!(read_info(&p).is_none());
        assert!(read_info(&dir.path().join("missing.gguf")).is_none());
    }

    /// Hand-built minimal GGUF: magic, v3, no tensors, two KV pairs.
    #[test]
    fn a_minimal_header_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("tiny-Q4_K_M.gguf");

        let mut b = Vec::new();
        b.extend_from_slice(MAGIC);
        b.extend_from_slice(&3u32.to_le_bytes()); // version
        b.extend_from_slice(&0u64.to_le_bytes()); // tensor count
        b.extend_from_slice(&2u64.to_le_bytes()); // kv count

        let kv_str = |k: &str, v: &str, out: &mut Vec<u8>| {
            out.extend_from_slice(&(k.len() as u64).to_le_bytes());
            out.extend_from_slice(k.as_bytes());
            out.extend_from_slice(&8u32.to_le_bytes());
            out.extend_from_slice(&(v.len() as u64).to_le_bytes());
            out.extend_from_slice(v.as_bytes());
        };
        kv_str("general.architecture", "lfm2", &mut b);

        let k = "lfm2.context_length";
        b.extend_from_slice(&(k.len() as u64).to_le_bytes());
        b.extend_from_slice(k.as_bytes());
        b.extend_from_slice(&10u32.to_le_bytes()); // uint64
        b.extend_from_slice(&128_000u64.to_le_bytes());

        std::fs::write(&p, b).unwrap();
        let info = read_info(&p).expect("a well-formed header must parse");
        assert_eq!(info.architecture.as_deref(), Some("lfm2"));
        assert_eq!(info.context_length, Some(128_000));
        assert_eq!(info.quantization.as_deref(), Some("Q4_K_M"));
    }
}
