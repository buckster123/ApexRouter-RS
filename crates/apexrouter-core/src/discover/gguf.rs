//! OWNER: unit C-10 (core/discover/models.rs, core/discover/gguf.rs). Do not edit outside
//! that unit.
//!
//! GGUF header reading. **Header only, bounded read** — never `mmap`, never more than
//! 8 MiB, because a discovery scan touches every weight file on the box and this laptop
//! lives under swap pressure.
//!
//! # How the bound is kept
//!
//! The parser walks the key/value block sequentially through a 64 KiB buffered reader and
//! *seeks* over payloads it does not want (long strings, tokenizer vocabularies, embedded
//! chat templates) rather than materialising them. Two counters are enforced on every
//! step: bytes actually pulled into memory, and the absolute file offset. Neither is ever
//! allowed past [`MAX_HEADER_BYTES`], so reading a 7 GB weights file costs a few kilobytes
//! of I/O.
//!
//! Arrays of strings are the one shape GGUF gives no way to skip — every element's length
//! has to be read to find the next one. A 250 000-token vocabulary is therefore the
//! natural stopping point: once `general.architecture` is known, meeting a long string
//! array ends the scan and we return what we have. Every field the fit solver needs is
//! written before the tokenizer block in practice. The cost is that a `general.*` key
//! placed *after* the vocabulary — some quantisers append `general.file_type` last, the
//! real `Carnice-9b-Q6_K.gguf` among them — is not seen, which is why
//! [`GgufMeta::quant_desc`] is optional and `LocalModel::quant`, derived from the
//! filename, is the user-facing value.

use crate::error::{Error, Result};
use apexrouter_protocol::GgufMeta;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

/// Hard ceiling on both bytes materialised and file offset reached. Never exceeded.
pub(crate) const MAX_HEADER_BYTES: u64 = 8 * 1024 * 1024;

/// The four magic bytes every GGUF file starts with.
const GGUF_MAGIC: [u8; 4] = *b"GGUF";

/// Longest metadata key we will hold in memory. Real keys are under 64 bytes.
const MAX_KEY_BYTES: u64 = 1024;

/// Longest string *value* we will hold in memory. Longer ones are seeked over.
const MAX_STRING_BYTES: u64 = 64 * 1024;

/// Longest array we will materialise. Per-layer arrays are `n_layer` long.
const MAX_ARRAY_ELEMS: u64 = 4096;

/// A string array longer than this ends the scan once the architecture is known — see the
/// module docs for why it cannot be skipped.
const STRING_ARRAY_STOP: u64 = 64;

// GGUF value type tags.
const T_UINT8: u32 = 0;
const T_INT8: u32 = 1;
const T_UINT16: u32 = 2;
const T_INT16: u32 = 3;
const T_UINT32: u32 = 4;
const T_INT32: u32 = 5;
const T_FLOAT32: u32 = 6;
const T_BOOL: u32 = 7;
const T_STRING: u32 = 8;
const T_ARRAY: u32 = 9;
const T_UINT64: u32 = 10;
const T_INT64: u32 = 11;
const T_FLOAT64: u32 = 12;

/// Read the fields the fit solver needs out of a GGUF header.
///
/// Handles typed KV, array and string values. Extracts `n_layer`, `n_head_kv`,
/// `n_embd_head_k/v`, `n_ctx_train` and, when present, `full_attn_layers` — hybrid-linear
/// models like Qwen3.6 MoE carry KV on only 10 of 41 layers, and assuming otherwise
/// over-estimates the cache by 4×.
///
/// # Errors
///
/// [`Error::Io`] when the file cannot be opened; [`Error::Invalid`] when it is not GGUF,
/// when its version is neither 2 nor 3, or when the header is too damaged to yield
/// `general.architecture`. A header merely *truncated* after the architecture is reported
/// with the fields that were readable rather than as a failure — a discovery scan meeting
/// a half-downloaded file is normal.
pub fn read_gguf_meta(path: &Path) -> Result<GgufMeta> {
    read_gguf_meta_bounded(path).map(|(meta, _)| meta)
}

/// What a bounded read actually cost. Test-visible proof of the 8 MiB promise.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct HeaderCost {
    /// Bytes pulled into memory.
    bytes_read: u64,
    /// Furthest offset reached in the file.
    end_offset: u64,
}

/// [`read_gguf_meta`] plus the accounting, so a test can assert the bound literally.
fn read_gguf_meta_bounded(path: &Path) -> Result<(GgufMeta, HeaderCost)> {
    let mut r = HeaderReader::open(path)?;

    let magic = r.take(4)?;
    if magic.as_slice() != GGUF_MAGIC {
        return Err(Error::Invalid {
            what: format!("gguf file {}", path.display()),
            why: "missing GGUF magic".to_owned(),
        });
    }
    let version = r.u32()?;
    if !(2..=3).contains(&version) {
        return Err(Error::Invalid {
            what: format!("gguf file {}", path.display()),
            why: format!("unsupported GGUF version {version} (this build reads 2 and 3)"),
        });
    }
    let _tensor_count = r.u64()?;
    let kv_count = r.u64()?;

    let mut kv: HashMap<String, Val> = HashMap::new();
    let mut have_arch = false;

    for _ in 0..kv_count {
        // A read failure past this point means a truncated or damaged header: keep what we
        // already decoded rather than throwing the whole file away.
        let key = match r.string(MAX_KEY_BYTES) {
            Ok(Some(k)) => k,
            Ok(None) | Err(_) => break,
        };
        let Ok(ty) = r.u32() else { break };
        let want = is_interesting(&key);

        if ty == T_ARRAY {
            let (Ok(elem_ty), Ok(len)) = (r.u32(), r.u64()) else {
                break;
            };
            if elem_ty == T_STRING && len > STRING_ARRAY_STOP && have_arch {
                // The vocabulary. Unskippable, and everything we need precedes it.
                break;
            }
            match read_array_body(&mut r, elem_ty, len, want) {
                Ok(Some(v)) => {
                    kv.insert(key, v);
                }
                Ok(None) => {}
                Err(_) => break,
            }
        } else {
            match read_scalar(&mut r, ty, want) {
                Ok(Some(v)) => {
                    if key == "general.architecture" {
                        have_arch = true;
                    }
                    kv.insert(key, v);
                }
                Ok(None) => {}
                Err(_) => break,
            }
        }
    }

    let meta = resolve(&kv).ok_or_else(|| Error::Invalid {
        what: format!("gguf file {}", path.display()),
        why: "header carries no general.architecture".to_owned(),
    })?;
    Ok((meta, r.cost()))
}

/// A metadata value we kept. Everything else is consumed and discarded.
#[derive(Clone, Debug, PartialEq)]
enum Val {
    /// Any integer or bool, widened.
    Num(u64),
    /// Any integer or bool array, widened. Per-layer `head_count_kv` arrives this way.
    Nums(Vec<u64>),
    /// A string value.
    Str(String),
}

/// Is this a key worth holding in memory? Everything else is parsed and thrown away.
///
/// Suffix matching (rather than `<arch>.` prefix matching) is deliberate: the architecture
/// name prefixes nearly every key, and is not guaranteed to be the *first* key in the file.
fn is_interesting(key: &str) -> bool {
    const SUFFIXES: &[&str] = &[
        ".block_count",
        ".context_length",
        ".embedding_length",
        ".attention.head_count",
        ".attention.head_count_kv",
        ".attention.key_length",
        ".attention.value_length",
        ".expert_count",
        ".full_attention_interval",
        ".full_attention_layers",
        ".recurrent_layer_arr",
    ];
    key.starts_with("general.") || SUFFIXES.iter().any(|s| key.ends_with(s))
}

/// Consume one non-array value. `None` means it was parsed and discarded.
fn read_scalar(r: &mut HeaderReader, ty: u32, want: bool) -> Result<Option<Val>> {
    match ty {
        T_UINT8 | T_BOOL => {
            let b = r.take(1)?;
            let v = u64::from(*b.first().unwrap_or(&0));
            Ok(want.then_some(Val::Num(v)))
        }
        T_INT8 => {
            let b = r.take(1)?;
            let v = i64::from(*b.first().unwrap_or(&0) as i8).max(0) as u64;
            Ok(want.then_some(Val::Num(v)))
        }
        T_UINT16 => {
            let v = r.u16()?;
            Ok(want.then_some(Val::Num(u64::from(v))))
        }
        T_INT16 => {
            let v = i64::from(r.u16()? as i16).max(0) as u64;
            Ok(want.then_some(Val::Num(v)))
        }
        T_UINT32 => {
            let v = r.u32()?;
            Ok(want.then_some(Val::Num(u64::from(v))))
        }
        T_INT32 => {
            let v = i64::from(r.u32()? as i32).max(0) as u64;
            Ok(want.then_some(Val::Num(v)))
        }
        T_UINT64 => {
            let v = r.u64()?;
            Ok(want.then_some(Val::Num(v)))
        }
        T_INT64 => {
            let v = (r.u64()? as i64).max(0) as u64;
            Ok(want.then_some(Val::Num(v)))
        }
        T_FLOAT32 => {
            r.skip(4)?;
            Ok(None)
        }
        T_FLOAT64 => {
            r.skip(8)?;
            Ok(None)
        }
        T_STRING => {
            let limit = if want { MAX_STRING_BYTES } else { 0 };
            Ok(r.string(limit)?.map(Val::Str))
        }
        other => Err(Error::Invalid {
            what: "gguf value".to_owned(),
            why: format!("unknown type tag {other}"),
        }),
    }
}

/// Consume one array body, its element type and length already read.
fn read_array_body(
    r: &mut HeaderReader,
    elem_ty: u32,
    len: u64,
    want: bool,
) -> Result<Option<Val>> {
    if elem_ty == T_ARRAY {
        return Err(Error::Invalid {
            what: "gguf array".to_owned(),
            why: "nested arrays are not part of GGUF".to_owned(),
        });
    }
    if elem_ty == T_STRING {
        // No byte count for the array as a whole: every element must be walked.
        for _ in 0..len {
            r.string(0)?;
        }
        return Ok(None);
    }
    let Some(width) = elem_width(elem_ty) else {
        return Err(Error::Invalid {
            what: "gguf array".to_owned(),
            why: format!("unknown element type tag {elem_ty}"),
        });
    };
    let total = len.saturating_mul(width);
    if !want || len > MAX_ARRAY_ELEMS {
        r.skip(total)?;
        return Ok(None);
    }
    let bytes = r.take(total)?;
    let mut out = Vec::with_capacity(len as usize);
    for chunk in bytes.chunks_exact(width as usize) {
        out.push(widen(elem_ty, chunk)?);
    }
    Ok(Some(Val::Nums(out)))
}

/// Byte width of a fixed-size GGUF element type.
fn elem_width(ty: u32) -> Option<u64> {
    match ty {
        T_UINT8 | T_INT8 | T_BOOL => Some(1),
        T_UINT16 | T_INT16 => Some(2),
        T_UINT32 | T_INT32 | T_FLOAT32 => Some(4),
        T_UINT64 | T_INT64 | T_FLOAT64 => Some(8),
        _ => None,
    }
}

/// Widen one fixed-size element to `u64`. Negatives and floats clamp to 0 — no field we
/// care about is either.
fn widen(ty: u32, b: &[u8]) -> Result<u64> {
    let bad = || Error::Invalid {
        what: "gguf array element".to_owned(),
        why: "short read".to_owned(),
    };
    Ok(match ty {
        T_UINT8 | T_BOOL => u64::from(*b.first().ok_or_else(bad)?),
        T_INT8 => i64::from(*b.first().ok_or_else(bad)? as i8).max(0) as u64,
        T_UINT16 => u64::from(u16::from_le_bytes(le::<2>(b)?)),
        T_INT16 => i64::from(i16::from_le_bytes(le::<2>(b)?)).max(0) as u64,
        T_UINT32 => u64::from(u32::from_le_bytes(le::<4>(b)?)),
        T_INT32 => i64::from(i32::from_le_bytes(le::<4>(b)?)).max(0) as u64,
        T_UINT64 => u64::from_le_bytes(le::<8>(b)?),
        T_INT64 => i64::from_le_bytes(le::<8>(b)?).max(0) as u64,
        T_FLOAT32 | T_FLOAT64 => 0,
        _ => return Err(bad()),
    })
}

/// Fixed-size little-endian slice, without `unwrap`.
fn le<const N: usize>(b: &[u8]) -> Result<[u8; N]> {
    b.get(..N)
        .and_then(|s| <[u8; N]>::try_from(s).ok())
        .ok_or_else(|| Error::Invalid {
            what: "gguf value".to_owned(),
            why: format!("short read: wanted {N} bytes"),
        })
}

/// Turn the harvested key/value map into the struct the fit solver consumes.
///
/// `None` when there is no architecture — the one field we refuse to guess at.
fn resolve(kv: &HashMap<String, Val>) -> Option<GgufMeta> {
    let arch = match kv.get("general.architecture") {
        Some(Val::Str(s)) if !s.is_empty() => s.clone(),
        _ => return None,
    };

    let num = |suffix: &str| lookup_num(kv, &arch, suffix);

    let n_layer = u32try(num(".block_count"));
    let n_ctx_train = u32try(num(".context_length"));
    let n_head = num(".attention.head_count").unwrap_or(0);
    let n_embd = num(".embedding_length").unwrap_or(0);
    let n_head_kv = u32try(num(".attention.head_count_kv"));

    // llama.cpp derives the head dimensions from the embedding width when the header does
    // not state them outright.
    let derived = n_embd.checked_div(n_head).unwrap_or(0);
    let n_embd_head_k = u32try(num(".attention.key_length").or(Some(derived)));
    let n_embd_head_v = u32try(num(".attention.value_length").or(Some(derived)));

    let n_expert = num(".expert_count").filter(|v| *v > 0).map(u32sat);
    let full_attn_layers = full_attention_layers(kv, &arch, n_layer);

    let quant_desc = match kv.get("general.file_type") {
        Some(Val::Num(v)) => file_type_name(*v).map(str::to_owned),
        _ => None,
    };

    Some(GgufMeta {
        arch,
        n_layer,
        n_head_kv,
        n_embd_head_k,
        n_embd_head_v,
        n_ctx_train,
        full_attn_layers,
        n_expert,
        quant_desc,
    })
}

/// A scalar metadata value: `<arch><suffix>` first, then any key carrying that suffix,
/// because a converter occasionally writes a prefix other than `general.architecture`
/// claims. An array collapses to its maximum, which is what `n_head_kv` means for a
/// per-layer array.
fn lookup_num(kv: &HashMap<String, Val>, arch: &str, suffix: &str) -> Option<u64> {
    match kv.get(&format!("{arch}{suffix}")) {
        Some(Val::Num(v)) => return Some(*v),
        Some(Val::Nums(v)) => return v.iter().copied().max(),
        _ => {}
    }
    kv.iter().find_map(|(k, v)| match v {
        Val::Num(n) if k.ends_with(suffix) => Some(*n),
        Val::Nums(n) if k.ends_with(suffix) => n.iter().copied().max(),
        _ => None,
    })
}

/// The array form of [`lookup_num`], for the per-layer metadata hybrids carry.
fn lookup_list<'a>(kv: &'a HashMap<String, Val>, arch: &str, suffix: &str) -> Option<&'a Vec<u64>> {
    if let Some(Val::Nums(v)) = kv.get(&format!("{arch}{suffix}")) {
        return Some(v);
    }
    kv.iter().find_map(|(k, v)| match v {
        Val::Nums(n) if k.ends_with(suffix) => Some(n),
        _ => None,
    })
}

/// How many layers actually carry a KV cache. `None` means "all of them".
///
/// Four shapes are seen in the wild, in order of authority: a per-layer
/// `attention.head_count_kv` array whose linear layers are 0; an explicit
/// `full_attention_layers` count; a `full_attention_interval` (the Qwen3.5/3.6 hybrids —
/// `Carnice-9b` has 32 blocks and interval 4, so 8 layers hold KV); a `recurrent_layer_arr`
/// boolean array whose `false` entries are the attention layers.
fn full_attention_layers(kv: &HashMap<String, Val>, arch: &str, n_layer: u32) -> Option<u32> {
    let num = |suffix: &str| lookup_num(kv, arch, suffix);
    let list = |suffix: &str| lookup_list(kv, arch, suffix);

    if let Some(per_layer) = list(".attention.head_count_kv") {
        if per_layer.len() > 1 {
            return Some(u32sat(per_layer.iter().filter(|v| **v > 0).count() as u64));
        }
    }
    if let Some(explicit) = num(".full_attention_layers") {
        if explicit > 0 {
            return Some(u32sat(explicit));
        }
    }
    if let Some(interval) = num(".full_attention_interval") {
        if interval > 1 && n_layer > 0 {
            return Some(u32sat((u64::from(n_layer) / interval).max(1)));
        }
    }
    if let Some(recurrent) = list(".recurrent_layer_arr") {
        if recurrent.len() > 1 {
            return Some(u32sat(recurrent.iter().filter(|v| **v == 0).count() as u64));
        }
    }
    None
}

/// `u32` or 0 — a missing count is 0, never a panic.
fn u32try(v: Option<u64>) -> u32 {
    v.map(u32sat).unwrap_or(0)
}

/// Saturating `u64 -> u32`.
fn u32sat(v: u64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

/// `general.file_type` is llama.cpp's `llama_ftype` enum. Name the ones that exist; an
/// unknown value is reported as `None` rather than invented.
fn file_type_name(v: u64) -> Option<&'static str> {
    Some(match v {
        0 => "F32",
        1 => "F16",
        2 => "Q4_0",
        3 => "Q4_1",
        7 => "Q8_0",
        8 => "Q5_0",
        9 => "Q5_1",
        10 => "Q2_K",
        11 => "Q3_K_S",
        12 => "Q3_K_M",
        13 => "Q3_K_L",
        14 => "Q4_K_S",
        15 => "Q4_K_M",
        16 => "Q5_K_S",
        17 => "Q5_K_M",
        18 => "Q6_K",
        19 => "IQ2_XXS",
        20 => "IQ2_XS",
        21 => "Q2_K_S",
        22 => "IQ3_XS",
        23 => "IQ3_XXS",
        24 => "IQ1_S",
        25 => "IQ4_NL",
        26 => "IQ3_S",
        27 => "IQ3_M",
        28 => "IQ2_S",
        29 => "IQ2_M",
        30 => "IQ4_XS",
        31 => "IQ1_M",
        32 => "BF16",
        36 => "TQ1_0",
        37 => "TQ2_0",
        _ => return None,
    })
}

/// A buffered, budgeted, seek-capable reader over a weights file.
///
/// Everything the parser does goes through `take` (materialise) or `skip` (seek past), and
/// both refuse to cross [`MAX_HEADER_BYTES`]. That is what makes the bound structural
/// rather than a promise in a comment.
struct HeaderReader {
    src: BufReader<File>,
    path: String,
    pos: u64,
    materialised: u64,
}

impl HeaderReader {
    /// Open a file for header reading with a 64 KiB window.
    fn open(path: &Path) -> Result<HeaderReader> {
        let f = File::open(path).map_err(|source| Error::Io {
            path: path.display().to_string(),
            source,
        })?;
        Ok(HeaderReader {
            src: BufReader::with_capacity(64 * 1024, f),
            path: path.display().to_string(),
            pos: 0,
            materialised: 0,
        })
    }

    /// What the read has cost so far.
    fn cost(&self) -> HeaderCost {
        HeaderCost {
            bytes_read: self.materialised,
            end_offset: self.pos,
        }
    }

    /// The budget error, in one place so its wording stays consistent.
    fn over_budget(&self, n: u64) -> Error {
        Error::Invalid {
            what: format!("gguf header of {}", self.path),
            why: format!(
                "bounded header read exhausted: {n} more bytes at offset {} would pass the \
                 {MAX_HEADER_BYTES} byte limit",
                self.pos
            ),
        }
    }

    /// Pull `n` bytes into memory. The budget is checked *before* allocating, so a header
    /// claiming a 4 GiB string cannot make us allocate 4 GiB.
    fn take(&mut self, n: u64) -> Result<Vec<u8>> {
        if self.pos.saturating_add(n) > MAX_HEADER_BYTES
            || self.materialised.saturating_add(n) > MAX_HEADER_BYTES
        {
            return Err(self.over_budget(n));
        }
        let mut buf = vec![0u8; n as usize];
        self.src.read_exact(&mut buf).map_err(|source| Error::Io {
            path: self.path.clone(),
            source,
        })?;
        self.pos += n;
        self.materialised += n;
        Ok(buf)
    }

    /// Seek past `n` bytes without materialising them.
    fn skip(&mut self, n: u64) -> Result<()> {
        if self.pos.saturating_add(n) > MAX_HEADER_BYTES {
            return Err(self.over_budget(n));
        }
        let by = i64::try_from(n).map_err(|_| self.over_budget(n))?;
        self.src.seek_relative(by).map_err(|source| Error::Io {
            path: self.path.clone(),
            source,
        })?;
        self.pos += n;
        Ok(())
    }

    /// A `u16` in little-endian order.
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(le::<2>(&self.take(2)?)?))
    }

    /// A `u32` in little-endian order.
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(le::<4>(&self.take(4)?)?))
    }

    /// A `u64` in little-endian order.
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(le::<8>(&self.take(8)?)?))
    }

    /// A length-prefixed GGUF string. Strings longer than `limit` are seeked over and
    /// reported as `None`; pass `0` to always skip the body.
    fn string(&mut self, limit: u64) -> Result<Option<String>> {
        let len = self.u64()?;
        if len > limit {
            self.skip(len)?;
            return Ok(None);
        }
        let bytes = self.take(len)?;
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // ---- a minimal GGUF writer, so the tests own their fixtures ----------------------

    fn push_str(buf: &mut Vec<u8>, s: &str) {
        buf.extend_from_slice(&(s.len() as u64).to_le_bytes());
        buf.extend_from_slice(s.as_bytes());
    }

    fn kv_u32(buf: &mut Vec<u8>, key: &str, v: u32) {
        push_str(buf, key);
        buf.extend_from_slice(&T_UINT32.to_le_bytes());
        buf.extend_from_slice(&v.to_le_bytes());
    }

    fn kv_u64(buf: &mut Vec<u8>, key: &str, v: u64) {
        push_str(buf, key);
        buf.extend_from_slice(&T_UINT64.to_le_bytes());
        buf.extend_from_slice(&v.to_le_bytes());
    }

    fn kv_i32(buf: &mut Vec<u8>, key: &str, v: i32) {
        push_str(buf, key);
        buf.extend_from_slice(&T_INT32.to_le_bytes());
        buf.extend_from_slice(&v.to_le_bytes());
    }

    fn kv_f32(buf: &mut Vec<u8>, key: &str, v: f32) {
        push_str(buf, key);
        buf.extend_from_slice(&T_FLOAT32.to_le_bytes());
        buf.extend_from_slice(&v.to_le_bytes());
    }

    fn kv_bool(buf: &mut Vec<u8>, key: &str, v: bool) {
        push_str(buf, key);
        buf.extend_from_slice(&T_BOOL.to_le_bytes());
        buf.push(u8::from(v));
    }

    fn kv_str(buf: &mut Vec<u8>, key: &str, v: &str) {
        push_str(buf, key);
        buf.extend_from_slice(&T_STRING.to_le_bytes());
        push_str(buf, v);
    }

    fn kv_u32_array(buf: &mut Vec<u8>, key: &str, vs: &[u32]) {
        push_str(buf, key);
        buf.extend_from_slice(&T_ARRAY.to_le_bytes());
        buf.extend_from_slice(&T_UINT32.to_le_bytes());
        buf.extend_from_slice(&(vs.len() as u64).to_le_bytes());
        for v in vs {
            buf.extend_from_slice(&v.to_le_bytes());
        }
    }

    fn kv_bool_array(buf: &mut Vec<u8>, key: &str, vs: &[bool]) {
        push_str(buf, key);
        buf.extend_from_slice(&T_ARRAY.to_le_bytes());
        buf.extend_from_slice(&T_BOOL.to_le_bytes());
        buf.extend_from_slice(&(vs.len() as u64).to_le_bytes());
        for v in vs {
            buf.push(u8::from(*v));
        }
    }

    fn kv_str_array(buf: &mut Vec<u8>, key: &str, n: usize, elem: &str) {
        push_str(buf, key);
        buf.extend_from_slice(&T_ARRAY.to_le_bytes());
        buf.extend_from_slice(&T_STRING.to_le_bytes());
        buf.extend_from_slice(&(n as u64).to_le_bytes());
        for _ in 0..n {
            push_str(buf, elem);
        }
    }

    /// Wrap already-encoded key/value records in a GGUF v3 header.
    fn gguf(kv_count: u64, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(body.len() + 24);
        out.extend_from_slice(&GGUF_MAGIC);
        out.extend_from_slice(&3u32.to_le_bytes());
        out.extend_from_slice(&427u64.to_le_bytes());
        out.extend_from_slice(&kv_count.to_le_bytes());
        out.extend_from_slice(body);
        out
    }

    fn write_tmp(name: &str, bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(name);
        let mut f = File::create(&path).expect("create");
        f.write_all(bytes).expect("write");
        f.sync_all().expect("sync");
        (dir, path)
    }

    // ---- tests -----------------------------------------------------------------------

    #[test]
    fn parses_typed_kv_string_and_array_values() {
        let mut b = Vec::new();
        kv_str(&mut b, "general.architecture", "llama");
        kv_str(&mut b, "general.name", "test");
        kv_u32(&mut b, "general.file_type", 15);
        kv_u32(&mut b, "llama.block_count", 32);
        kv_u64(&mut b, "llama.context_length", 32768);
        kv_i32(&mut b, "llama.attention.head_count", 32);
        kv_u32(&mut b, "llama.attention.head_count_kv", 8);
        kv_u32(&mut b, "llama.attention.key_length", 128);
        kv_u32(&mut b, "llama.attention.value_length", 128);
        kv_u32(&mut b, "llama.expert_count", 8);
        // Types we must consume correctly even though we discard them.
        kv_f32(&mut b, "llama.rope.freq_base", 1.0e7);
        kv_bool(&mut b, "llama.some.flag", true);
        kv_u32_array(&mut b, "llama.rope.dimension_sections", &[11, 11, 10, 0]);
        kv_str_array(&mut b, "general.tags", 3, "tag");

        let (_d, p) = write_tmp("typed.gguf", &gguf(14, &b));
        let m = read_gguf_meta(&p).expect("parses");
        assert_eq!(m.arch, "llama");
        assert_eq!(m.n_layer, 32);
        assert_eq!(m.n_ctx_train, 32768);
        assert_eq!(m.n_head_kv, 8);
        assert_eq!(m.n_embd_head_k, 128);
        assert_eq!(m.n_embd_head_v, 128);
        assert_eq!(m.n_expert, Some(8));
        assert_eq!(m.full_attn_layers, None, "a dense model uses every layer");
        assert_eq!(m.quant_desc.as_deref(), Some("Q4_K_M"));
    }

    #[test]
    fn head_dims_are_derived_when_the_header_omits_them() {
        let mut b = Vec::new();
        kv_str(&mut b, "general.architecture", "llama");
        kv_u32(&mut b, "llama.block_count", 28);
        kv_u32(&mut b, "llama.context_length", 4096);
        kv_u32(&mut b, "llama.embedding_length", 4096);
        kv_u32(&mut b, "llama.attention.head_count", 32);
        kv_u32(&mut b, "llama.attention.head_count_kv", 8);
        let (_d, p) = write_tmp("derive.gguf", &gguf(6, &b));

        let m = read_gguf_meta(&p).expect("parses");
        assert_eq!(m.n_embd_head_k, 128, "4096 / 32");
        assert_eq!(m.n_embd_head_v, 128);
    }

    #[test]
    fn hybrid_kv_layers_come_from_a_per_layer_head_count_array() {
        // Qwen3.6-MoE shape: 41 blocks, KV on 10 of them.
        let mut per_layer = vec![0u32; 41];
        for (i, v) in per_layer.iter_mut().enumerate() {
            if i % 4 == 3 {
                *v = 4;
            }
        }
        let expected = per_layer.iter().filter(|v| **v > 0).count() as u32;
        assert_eq!(expected, 10);

        let mut b = Vec::new();
        kv_str(&mut b, "general.architecture", "qwen3next");
        kv_u32(&mut b, "qwen3next.block_count", 41);
        kv_u32(&mut b, "qwen3next.context_length", 262144);
        kv_u32_array(&mut b, "qwen3next.attention.head_count_kv", &per_layer);
        kv_u32(&mut b, "qwen3next.attention.key_length", 256);
        kv_u32(&mut b, "qwen3next.attention.value_length", 256);
        let (_d, p) = write_tmp("hybrid.gguf", &gguf(6, &b));

        let m = read_gguf_meta(&p).expect("parses");
        assert_eq!(m.n_layer, 41);
        assert_eq!(m.n_head_kv, 4, "the array's non-zero width, not its length");
        assert_eq!(m.full_attn_layers, Some(expected));
    }

    #[test]
    fn hybrid_kv_layers_come_from_a_full_attention_interval() {
        let mut b = Vec::new();
        kv_str(&mut b, "general.architecture", "qwen35");
        kv_u32(&mut b, "qwen35.block_count", 32);
        kv_u32(&mut b, "qwen35.context_length", 262144);
        kv_u32(&mut b, "qwen35.attention.head_count_kv", 4);
        kv_u32(&mut b, "qwen35.full_attention_interval", 4);
        let (_d, p) = write_tmp("interval.gguf", &gguf(5, &b));

        let m = read_gguf_meta(&p).expect("parses");
        assert_eq!(m.full_attn_layers, Some(8), "32 blocks / interval 4");
    }

    #[test]
    fn hybrid_kv_layers_come_from_a_recurrent_layer_array() {
        let recurrent: Vec<bool> = (0..12).map(|i| i % 3 != 2).collect();
        let mut b = Vec::new();
        kv_str(&mut b, "general.architecture", "jamba");
        kv_u32(&mut b, "jamba.block_count", 12);
        kv_u32(&mut b, "jamba.attention.head_count_kv", 8);
        kv_bool_array(&mut b, "jamba.recurrent_layer_arr", &recurrent);
        let (_d, p) = write_tmp("recurrent.gguf", &gguf(4, &b));

        let m = read_gguf_meta(&p).expect("parses");
        assert_eq!(m.full_attn_layers, Some(4), "the four non-recurrent layers");
    }

    #[test]
    fn a_vocabulary_sized_header_never_reads_more_than_eight_mib() {
        // 400 000 tokens of 24 bytes each: ~12 MB of payload after the fields we need.
        let mut b = Vec::new();
        kv_str(&mut b, "general.architecture", "llama");
        kv_u32(&mut b, "llama.block_count", 32);
        kv_u32(&mut b, "llama.context_length", 32768);
        kv_u32(&mut b, "llama.attention.head_count_kv", 8);
        kv_u32(&mut b, "llama.attention.key_length", 128);
        kv_u32(&mut b, "llama.attention.value_length", 128);
        kv_str_array(
            &mut b,
            "tokenizer.ggml.tokens",
            400_000,
            "aaaaaaaaaaaaaaaaaaaaaaaa",
        );
        kv_u32(&mut b, "general.file_type", 18);
        let bytes = gguf(8, &b);
        assert!(bytes.len() as u64 > MAX_HEADER_BYTES, "fixture must be big");

        let (_d, p) = write_tmp("vocab.gguf", &bytes);
        let (m, cost) = read_gguf_meta_bounded(&p).expect("parses");

        assert_eq!(m.n_layer, 32);
        assert_eq!(m.n_head_kv, 8);
        assert_eq!(m.n_embd_head_k, 128);
        assert!(
            cost.bytes_read <= MAX_HEADER_BYTES,
            "materialised {} bytes",
            cost.bytes_read
        );
        assert!(
            cost.end_offset <= MAX_HEADER_BYTES,
            "reached offset {}",
            cost.end_offset
        );
        // And in practice it is a rounding error, not the ceiling.
        assert!(
            cost.end_offset < 64 * 1024,
            "reached offset {}",
            cost.end_offset
        );
    }

    #[test]
    fn a_lying_length_is_refused_rather_than_allocated() {
        let mut b = Vec::new();
        push_str(&mut b, "general.architecture");
        b.extend_from_slice(&T_STRING.to_le_bytes());
        b.extend_from_slice(&(4u64 << 30).to_le_bytes()); // claims 4 GiB
        b.extend_from_slice(b"llama");
        let (_d, p) = write_tmp("liar.gguf", &gguf(1, &b));

        let err = read_gguf_meta(&p).expect_err("must not allocate 4 GiB");
        assert!(matches!(err, Error::Invalid { .. }), "{err}");
    }

    #[test]
    fn a_non_gguf_file_is_rejected() {
        let (_d, p) = write_tmp("not.gguf", b"this is a text file, honestly");
        let err = read_gguf_meta(&p).expect_err("bad magic");
        assert!(err.to_string().contains("GGUF magic"), "{err}");
    }

    #[test]
    fn an_unsupported_version_is_named_in_the_error() {
        let mut out = Vec::new();
        out.extend_from_slice(&GGUF_MAGIC);
        out.extend_from_slice(&9u32.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes());
        let (_d, p) = write_tmp("v9.gguf", &out);
        let err = read_gguf_meta(&p).expect_err("bad version");
        assert!(err.to_string().contains('9'), "{err}");
    }

    #[test]
    fn a_header_truncated_after_the_architecture_still_reports_what_it_had() {
        let mut b = Vec::new();
        kv_str(&mut b, "general.architecture", "llama");
        kv_u32(&mut b, "llama.block_count", 32);
        // Claim four more pairs than are actually present.
        let (_d, p) = write_tmp("short.gguf", &gguf(6, &b));

        let m = read_gguf_meta(&p).expect("a partial header is still useful");
        assert_eq!(m.arch, "llama");
        assert_eq!(m.n_layer, 32);
    }

    #[test]
    fn a_missing_file_is_an_io_error_naming_the_path() {
        let err = read_gguf_meta(Path::new("/nonexistent/nowhere.gguf")).expect_err("no file");
        assert!(err.to_string().contains("nowhere.gguf"), "{err}");
    }

    /// The real machine: `~/models/carnice-9b/Carnice-9b-Q6_K.gguf` is the only complete
    /// GGUF on this box (`docs/port/00-machine-ground-truth.md`). Skipped elsewhere.
    #[test]
    fn the_real_carnice_header_parses() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let p = home.join("models/carnice-9b/Carnice-9b-Q6_K.gguf");
        if !p.is_file() {
            return;
        }
        let (m, cost) = read_gguf_meta_bounded(&p).expect("real header parses");
        assert_eq!(m.arch, "qwen35");
        assert_eq!(m.n_layer, 32);
        assert_eq!(m.n_head_kv, 4);
        assert_eq!(m.n_embd_head_k, 256);
        assert_eq!(m.n_embd_head_v, 256);
        assert_eq!(m.n_ctx_train, 262_144);
        assert_eq!(
            m.full_attn_layers,
            Some(8),
            "32 blocks, full_attention_interval 4"
        );
        assert!(
            cost.bytes_read <= MAX_HEADER_BYTES && cost.end_offset <= MAX_HEADER_BYTES,
            "read {} bytes, reached offset {} of a 7.3 GB file",
            cost.bytes_read,
            cost.end_offset
        );
    }
}
