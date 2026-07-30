//! OWNER: unit R-03 (router/src/relay/{mod,headers,body}.rs). Do not edit outside that unit.
//!
//! Request bodies. Two rules:
//!
//! * When the alias equals the upstream id, the body is [`BodyPlan::Passthrough`] — the
//!   original `Bytes`, zero copies. Otherwise it is [`BodyPlan::Rewritten`] and **only the
//!   `model` value changes**, which a tool-calling fixture round-trip asserts down to float
//!   formatting inside `tools[]`.
//! * [`peek`] is a top-level key **scanner**, not a `serde_json::Value` parse: a 4 MiB body
//!   must not allocate 4 MiB of DOM to learn whether `stream` is true.
//!
//! Both of those come from the same place: a small, allocation-free byte scanner over the
//! top level of the JSON object. A `serde_json` round-trip would be shorter to write and
//! wrong — it renumbers floats (`1e-7` becomes `1.0e-7`, `0.10` becomes `0.1`), reorders
//! nothing but reformats everything, and a tool-calling body that came back subtly different
//! is exactly the class of bug nobody finds until a model starts refusing to call tools.

use apexrouter_core::error::{Error, Result};
use bytes::Bytes;
use std::ops::Range;

/// What to send upstream.
pub enum BodyPlan {
    /// The original bytes, untouched.
    Passthrough(Bytes),
    /// The original bytes with exactly one value replaced.
    Rewritten(Bytes),
}

/// Decide whether the body needs rewriting, and do it if so.
///
/// `rewrite_model_to == None` (the alias *is* the upstream id, or the body is opaque) yields
/// [`BodyPlan::Passthrough`] holding a clone of `original` — a `Bytes` refcount bump, not a
/// copy. So does a body whose `model` already reads exactly as requested.
///
/// Otherwise the top-level `"model"` value is spliced out by byte range and the requested
/// name is spliced in, JSON-escaped. Every other byte of the document — key order,
/// whitespace, float spelling inside `tools[]`, unicode escapes — is carried across
/// untouched. When the body carries no top-level `"model"` at all, one is inserted as the
/// first member; a client that sent none cannot notice the difference, and a paid upstream
/// that requires one now gets it.
///
/// # Errors
/// [`Error::Invalid`] when a rewrite was asked for and the body is not a JSON object.
pub fn plan_body(original: &Bytes, rewrite_model_to: Option<&str>) -> Result<BodyPlan> {
    let Some(target) = rewrite_model_to else {
        return Ok(BodyPlan::Passthrough(original.clone()));
    };
    let b: &[u8] = original.as_ref();

    let mut span: Option<Range<usize>> = None;
    let scanned = scan_object(b, 0, &mut |key, value| {
        if key == b"model" {
            span = Some(value);
            false // found it; stop
        } else {
            true
        }
    });
    if scanned.is_none() {
        return Err(Error::Invalid {
            what: "request body".to_owned(),
            why: "expected a JSON object at the top level to rewrite `model` in".to_owned(),
        });
    }

    // `serde_json` for the *escaping* only: one short string, never the document.
    let encoded = serde_json::to_string(target)?;

    match span {
        Some(s) => {
            if &b[s.clone()] == encoded.as_bytes() {
                // Already exactly right — do not touch the bytes at all.
                return Ok(BodyPlan::Passthrough(original.clone()));
            }
            let mut out = Vec::with_capacity(b.len() - s.len() + encoded.len());
            out.extend_from_slice(&b[..s.start]);
            out.extend_from_slice(encoded.as_bytes());
            out.extend_from_slice(&b[s.end..]);
            Ok(BodyPlan::Rewritten(Bytes::from(out)))
        }
        None => {
            let (open, empty) = object_open(b).ok_or_else(|| Error::Invalid {
                what: "request body".to_owned(),
                why: "expected a JSON object at the top level to rewrite `model` in".to_owned(),
            })?;
            let mut out = Vec::with_capacity(b.len() + encoded.len() + 10);
            out.extend_from_slice(&b[..open]);
            out.extend_from_slice(b"\"model\":");
            out.extend_from_slice(encoded.as_bytes());
            if !empty {
                out.push(b',');
            }
            out.extend_from_slice(&b[open..]);
            Ok(BodyPlan::Rewritten(Bytes::from(out)))
        }
    }
}

/// The three things the request path needs to know before it can route.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RequestPeek {
    /// The `"model"` value, when there is one.
    pub model: Option<String>,
    /// `"stream"`, read **strictly** as a bool — `"stream": "true"` is not streaming.
    pub stream: bool,
    /// `"stream_options": {"include_usage": true}`.
    pub include_usage: bool,
    /// Body length, for the global byte budget.
    pub bytes: usize,
}

/// Top-level key scanner. Does NOT build a full `serde_json::Value`.
///
/// One left-to-right pass over the top level of the object, skipping every value it does not
/// care about by structure rather than by parsing it. The only allocation is the `model`
/// string itself, so a 4 MiB tool-calling body costs a few dozen bytes to inspect instead of
/// several megabytes of DOM — asserted by a test with a counting allocator.
///
/// Infallible by design: a malformed body still has to be *sent* somewhere so the upstream
/// can produce the error message the client deserves. A body this cannot parse simply yields
/// the fields it managed to read, and `bytes` is always the true length.
///
/// `stream` is read strictly: `"stream": "true"` (a string) is **not** streaming, which is
/// the shape a hand-rolled shell client sends and the shape LocalRouter used to get wrong.
pub fn peek(body: &[u8]) -> RequestPeek {
    let mut p = RequestPeek {
        bytes: body.len(),
        ..Default::default()
    };
    scan_object(body, 0, &mut |key, value| {
        match key {
            b"model" => {
                if body.get(value.start) == Some(&b'"') && value.len() >= 2 {
                    p.model = unescape(&body[value.start + 1..value.end - 1]);
                }
            }
            b"stream" => p.stream = &body[value] == b"true",
            b"stream_options" => {
                scan_object(body, value.start, &mut |k, v| {
                    if k == b"include_usage" {
                        p.include_usage = &body[v] == b"true";
                        false
                    } else {
                        true
                    }
                });
            }
            _ => {}
        }
        true
    });
    p
}

/// The first path segments that name the **OpenAI/Anthropic API surface** rather than a
/// server-native endpoint.
///
/// A client configured with `http://127.0.0.1:8888` (the form `README.md`, `tool_menus.py`
/// and ApexOS all use) sends `/chat/completions`, not `/v1/chat/completions`. LocalRouter
/// got that right by accident — it appended the client path onto a `base_url` that already
/// ended in `/v1`. ApexRouter stores every `base_url` **without** `/v1`, so the prefix has
/// to be restored here or the bare-base form 404s.
///
/// Deliberately plural-only: llama.cpp's own `/completion`, `/embedding`, `/infill`,
/// `/tokenize`, `/props`, `/metrics`, `/slots` and `/health` are **not** in this set and
/// forward raw, exactly as `ARCHITECTURE.md` §6.1 requires.
const API_SURFACE: &[&str] = &[
    "assistants",
    "audio",
    "batches",
    "chat",
    "completions",
    "embeddings",
    "files",
    "fine_tuning",
    "images",
    "messages",
    "moderations",
    "models",
    "rerank",
    "reranking",
    "responses",
    "threads",
    "uploads",
    "vector_stores",
];

/// Canonicalise the inbound path to **exactly one** leading `/v1` on the API surface, and
/// report whether a duplicate `/v1` was collapsed.
///
/// Both `http://127.0.0.1:8888` and `http://127.0.0.1:8888/v1` must work as client base
/// URLs — mandatory, because `smoke.sh` appends `/v1` to whatever you give it and the
/// project's own SKILL.md told agents to use the form that 404s today. Concretely:
///
/// | inbound | normalised | why |
/// |---|---|---|
/// | `/chat/completions` | `/v1/chat/completions` | base URL `…:8888` |
/// | `/v1/chat/completions` | `/v1/chat/completions` | base URL `…:8888/v1` |
/// | `/v1/v1/chat/completions` | `/v1/chat/completions` | `smoke.sh` against `…:8888/v1` |
/// | `/props` | `/props` | server-native, forwarded raw |
///
/// Only whole `/v1` **segments** at the front collapse: `/v1/v1beta/models` is left alone,
/// and `/v1/chat/v1/completions` is left alone. `/v1/v1/v1/models` collapses to `/v1/models`.
///
/// Returns `(normalized, collapsed_a_duplicate_v1)`. The flag is **only** about a doubled
/// prefix — adding a missing one is the documented-good client, not a broken one, and must
/// not be logged as a misconfiguration.
pub fn normalize_path(path: &str) -> (String, bool) {
    let mut rest = path;
    let mut collapsed = false;
    while leading_v1(rest) && leading_v1(&rest[3..]) {
        rest = &rest[3..];
        collapsed = true;
    }
    if !leading_v1(rest) && is_api_surface(rest) {
        return (format!("/v1{rest}"), collapsed);
    }
    (rest.to_owned(), collapsed)
}

/// Is the first segment of this `/`-rooted path part of the versioned API surface?
fn is_api_surface(path: &str) -> bool {
    let Some(rest) = path.strip_prefix('/') else {
        return false;
    };
    let head = rest.split('/').next().unwrap_or("");
    !head.is_empty() && API_SURFACE.contains(&head)
}

/// Does `s` begin with a whole `/v1` path segment?
fn leading_v1(s: &str) -> bool {
    s.starts_with("/v1") && (s.len() == 3 || s.as_bytes()[3] == b'/')
}

// ---------------------------------------------------------------------------------------
// The scanner. Byte ranges in, byte ranges out; no `serde_json::Value`, no allocation.
// ---------------------------------------------------------------------------------------

/// A cursor over a JSON document.
struct Scan<'a> {
    b: &'a [u8],
    i: usize,
}

impl Scan<'_> {
    fn at(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.at(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    /// Consume a string starting at the current `"`. Returns the span *inside* the quotes.
    fn string(&mut self) -> Option<Range<usize>> {
        if self.at()? != b'"' {
            return None;
        }
        self.i += 1;
        let start = self.i;
        loop {
            let c = self.at()?;
            self.i += 1;
            match c {
                b'\\' => {
                    self.at()?;
                    self.i += 1;
                }
                b'"' => return Some(start..self.i - 1),
                _ => {}
            }
        }
    }

    /// Consume one value. Returns its full span, quotes and braces included.
    fn value(&mut self) -> Option<Range<usize>> {
        self.skip_ws();
        let start = self.i;
        match self.at()? {
            b'"' => {
                self.string()?;
            }
            b'{' | b'[' => {
                let mut depth = 0usize;
                loop {
                    match self.at()? {
                        b'"' => {
                            self.string()?;
                        }
                        b'{' | b'[' => {
                            depth += 1;
                            self.i += 1;
                        }
                        b'}' | b']' => {
                            self.i += 1;
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => self.i += 1,
                    }
                }
            }
            _ => {
                while let Some(c) = self.at() {
                    if matches!(c, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r') {
                        break;
                    }
                    self.i += 1;
                }
                if self.i == start {
                    return None;
                }
            }
        }
        Some(start..self.i)
    }
}

/// Walk the members of the JSON object beginning at `from`, calling `f(key_bytes, value_span)`
/// for each. `f` returns `false` to stop early.
///
/// The key is handed over **raw**, exactly as it sits between its quotes: the keys this
/// scanner looks for (`model`, `stream`, `stream_options`, `include_usage`) contain nothing
/// escapable, so a `"model"` spelling simply does not match — and a client that writes
/// its keys that way gets whatever the upstream makes of them, which is the honest outcome.
///
/// Returns `None` if there was no object there, or the document ran out mid-member. Stopping
/// early via `f` is *not* a failure: it returns `Some`.
fn scan_object(
    b: &[u8],
    from: usize,
    f: &mut impl FnMut(&[u8], Range<usize>) -> bool,
) -> Option<()> {
    let mut s = Scan { b, i: from };
    s.skip_ws();
    if s.at()? != b'{' {
        return None;
    }
    s.i += 1;
    loop {
        s.skip_ws();
        match s.at()? {
            b'}' => return Some(()),
            b',' => {
                s.i += 1;
                continue;
            }
            b'"' => {}
            _ => return None,
        }
        let key = s.string()?;
        s.skip_ws();
        if s.at()? != b':' {
            return None;
        }
        s.i += 1;
        let value = s.value()?;
        if !f(&b[key], value) {
            return Some(());
        }
    }
}

/// Index just past the top-level `{`, and whether the object has no members.
fn object_open(b: &[u8]) -> Option<(usize, bool)> {
    let mut s = Scan { b, i: 0 };
    s.skip_ws();
    if s.at()? != b'{' {
        return None;
    }
    s.i += 1;
    let open = s.i;
    s.skip_ws();
    Some((open, s.at() == Some(b'}')))
}

/// Decode a JSON string body (the bytes between the quotes) into a `String`.
///
/// Returns `None` on an escape this does not understand, which is the same answer as "no
/// model in this body" — the router then falls through to its default alias rather than
/// routing on a half-decoded name.
fn unescape(raw: &[u8]) -> Option<String> {
    if !raw.contains(&b'\\') {
        return std::str::from_utf8(raw).ok().map(str::to_owned);
    }
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let c = raw[i];
        if c != b'\\' {
            let start = i;
            while i < raw.len() && raw[i] != b'\\' {
                i += 1;
            }
            out.push_str(std::str::from_utf8(&raw[start..i]).ok()?);
            continue;
        }
        i += 1;
        let e = *raw.get(i)?;
        i += 1;
        match e {
            b'"' => out.push('"'),
            b'\\' => out.push('\\'),
            b'/' => out.push('/'),
            b'b' => out.push('\u{8}'),
            b'f' => out.push('\u{c}'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'u' => {
                let hi = hex4(raw.get(i..i + 4)?)?;
                i += 4;
                let ch = if (0xD800..0xDC00).contains(&hi) {
                    // A surrogate pair: the low half must follow.
                    if raw.get(i..i + 2)? != b"\\u" {
                        return None;
                    }
                    i += 2;
                    let lo = hex4(raw.get(i..i + 4)?)?;
                    i += 4;
                    if !(0xDC00..0xE000).contains(&lo) {
                        return None;
                    }
                    let c = 0x1_0000 + ((u32::from(hi) - 0xD800) << 10) + (u32::from(lo) - 0xDC00);
                    char::from_u32(c)?
                } else {
                    char::from_u32(u32::from(hi))?
                };
                out.push(ch);
            }
            _ => return None,
        }
    }
    Some(out)
}

/// Four hex digits to a `u16`.
fn hex4(b: &[u8]) -> Option<u16> {
    let mut v: u16 = 0;
    for &c in b {
        let d = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => return None,
        };
        v = v.checked_mul(16)?.checked_add(u16::from(d))?;
    }
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    // ---- a per-thread allocation counter, so the "< 4 KiB" claim is measured, not asserted.
    //
    // The counter is thread-local and const-initialised: the allocator itself never
    // allocates, and tests running in parallel on other threads cannot pollute the reading.
    thread_local! {
        static ALLOCATED: Cell<usize> = const { Cell::new(0) };
    }

    struct Counting;

    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            let _ = ALLOCATED.try_with(|c| c.set(c.get().saturating_add(l.size())));
            System.alloc(l)
        }
        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            System.dealloc(p, l)
        }
        unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
            let _ = ALLOCATED.try_with(|c| c.set(c.get().saturating_add(l.size())));
            System.alloc_zeroed(l)
        }
        unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
            let _ =
                ALLOCATED.try_with(|c| c.set(c.get().saturating_add(new.saturating_sub(l.size()))));
            System.realloc(p, l, new)
        }
    }

    #[global_allocator]
    static ALLOC: Counting = Counting;

    fn allocated() -> usize {
        ALLOCATED.with(Cell::get)
    }

    /// A realistic tool-calling body, with float spellings `serde_json` would rewrite.
    const TOOLS_FIXTURE: &str = r#"{
  "model": "auto",
  "messages": [
    {"role": "system", "content": "You are terse."},
    {"role": "user", "content": "weather in Oslo?"}
  ],
  "temperature": 0.10,
  "top_p": 1.0,
  "frequency_penalty": 1e-7,
  "presence_penalty": -0.00,
  "seed": 9007199254740993,
  "tools": [
    {
      "type": "function",
      "function": {
        "name": "get_weather",
        "description": "Look up the weather. Handles a \"model\": \"decoy\" in the text.",
        "parameters": {
          "type": "object",
          "properties": {
            "city": {"type": "string"},
            "precision": {"type": "number", "default": 0.50, "minimum": 1e-3}
          },
          "required": ["city"]
        }
      }
    }
  ],
  "tool_choice": "auto",
  "stream": true,
  "stream_options": {"include_usage": true}
}"#;

    #[test]
    fn passthrough_is_the_same_allocation() {
        let original = Bytes::from_static(br#"{"model":"carnice-9b"}"#);
        let ptr = original.as_ptr();
        match plan_body(&original, None) {
            Ok(BodyPlan::Passthrough(b)) => {
                assert_eq!(b.as_ptr(), ptr, "passthrough copied the body");
                assert_eq!(b.len(), original.len());
            }
            _ => panic!("expected Passthrough"),
        }
    }

    #[test]
    fn rewriting_to_the_same_name_stays_a_passthrough() {
        let original = Bytes::from_static(br#"{"model":"carnice-9b","stream":false}"#);
        let ptr = original.as_ptr();
        match plan_body(&original, Some("carnice-9b")) {
            Ok(BodyPlan::Passthrough(b)) => assert_eq!(b.as_ptr(), ptr),
            _ => panic!("expected Passthrough"),
        }
    }

    /// The acceptance test: a rewrite changes the `model` value and **nothing else**,
    /// byte for byte, including float spelling inside `tools[]`.
    #[test]
    fn rewrite_changes_only_the_model_value() {
        let original = Bytes::from(TOOLS_FIXTURE.to_owned());
        let out = match plan_body(&original, Some("carnice-9b")) {
            Ok(BodyPlan::Rewritten(b)) => b,
            _ => panic!("expected Rewritten"),
        };
        let expected = TOOLS_FIXTURE.replacen(r#""model": "auto""#, r#""model": "carnice-9b""#, 1);
        assert_eq!(
            std::str::from_utf8(&out).expect("utf8"),
            expected,
            "a rewrite touched something other than the model value"
        );
        // Spelled out, in case the fixture ever changes: these survive verbatim.
        let s = std::str::from_utf8(&out).expect("utf8");
        for verbatim in [
            "0.10",
            "1.0",
            "1e-7",
            "-0.00",
            "9007199254740993",
            "0.50",
            "1e-3",
            r#"\"model\": \"decoy\""#,
        ] {
            assert!(s.contains(verbatim), "{verbatim} was reformatted");
        }
        // And the decoy inside the description was not the thing that got rewritten.
        assert_eq!(s.matches("carnice-9b").count(), 1);
    }

    #[test]
    fn rewrite_inserts_a_model_when_the_body_has_none() {
        let original = Bytes::from_static(br#"{"messages":[],"stream":false}"#);
        let out = match plan_body(&original, Some("carnice-9b")) {
            Ok(BodyPlan::Rewritten(b)) => b,
            _ => panic!("expected Rewritten"),
        };
        assert_eq!(
            std::str::from_utf8(&out).expect("utf8"),
            r#"{"model":"carnice-9b","messages":[],"stream":false}"#
        );

        let empty = Bytes::from_static(b"{}");
        let out = match plan_body(&empty, Some("a/b")) {
            Ok(BodyPlan::Rewritten(b)) => b,
            _ => panic!("expected Rewritten"),
        };
        assert_eq!(
            std::str::from_utf8(&out).expect("utf8"),
            r#"{"model":"a/b"}"#
        );
    }

    #[test]
    fn a_name_needing_escapes_is_escaped() {
        let original = Bytes::from_static(br#"{"model":"a"}"#);
        let out = match plan_body(&original, Some("a\"b\\c")) {
            Ok(BodyPlan::Rewritten(b)) => b,
            _ => panic!("expected Rewritten"),
        };
        let s = std::str::from_utf8(&out).expect("utf8");
        assert_eq!(s, r#"{"model":"a\"b\\c"}"#);
        // …and it round-trips back through a real parser to the exact name.
        let v: serde_json::Value = serde_json::from_str(s).expect("valid json");
        assert_eq!(v["model"], serde_json::json!("a\"b\\c"));
    }

    #[test]
    fn a_non_object_body_cannot_be_rewritten() {
        let original = Bytes::from_static(b"not json at all");
        assert!(plan_body(&original, Some("x")).is_err());
        // …but it relays fine when no rewrite was asked for.
        assert!(matches!(
            plan_body(&original, None),
            Ok(BodyPlan::Passthrough(_))
        ));
    }

    #[test]
    fn peek_reads_the_three_fields() {
        let p = peek(TOOLS_FIXTURE.as_bytes());
        assert_eq!(p.model.as_deref(), Some("auto"));
        assert!(p.stream);
        assert!(p.include_usage);
        assert_eq!(p.bytes, TOOLS_FIXTURE.len());
    }

    #[test]
    fn peek_reads_stream_strictly() {
        assert!(!peek(br#"{"stream":"true"}"#).stream);
        assert!(!peek(br#"{"stream":1}"#).stream);
        assert!(!peek(br#"{"stream":null}"#).stream);
        assert!(peek(br#"{ "stream" : true }"#).stream);
        assert!(!peek(br#"{"stream_options":{"include_usage":true}}"#).stream);
        assert!(!peek(br#"{"stream_options":{"include_usage":"true"}}"#).include_usage);
    }

    #[test]
    fn peek_is_not_fooled_by_nested_keys() {
        let body = br#"{"messages":[{"role":"user","content":"{\"model\":\"gpt-4\"}","model":"nested"}],"tools":[{"stream":true}],"model":"real","stream":false}"#;
        let p = peek(body);
        assert_eq!(p.model.as_deref(), Some("real"));
        assert!(!p.stream);
    }

    #[test]
    fn peek_survives_a_malformed_body() {
        let p = peek(br#"{"model":"a","messages":[{"#);
        assert_eq!(p.model.as_deref(), Some("a"));
        assert_eq!(peek(b"").bytes, 0);
        assert_eq!(peek(b"garbage"), RequestPeek::default().tap_bytes(7));
    }

    #[test]
    fn peek_decodes_escapes_in_the_model_name() {
        assert_eq!(
            peek(r#"{"model":"vendor\/model:8bé 🚀"}"#.as_bytes())
                .model
                .as_deref(),
            Some("vendor/model:8bé 🚀")
        );
        // \u escapes, including a surrogate pair.
        assert_eq!(
            peek(br#"{"model":"a\/b\ud83d\ude80"}"#).model.as_deref(),
            Some("a/b\u{1F680}")
        );
        // A broken escape is "no model", never a half-decoded name.
        assert_eq!(peek(br#"{"model":"a\uZZZZ"}"#).model, None);
    }

    /// The acceptance test: `peek` on a 4 MiB body allocates < 4 KiB.
    #[test]
    fn peek_on_a_four_mib_body_barely_allocates() {
        let mut body = String::with_capacity(5 << 20);
        body.push_str(r#"{"model":"carnice-9b","messages":[{"role":"user","content":""#);
        while body.len() < (4 << 20) {
            body.push_str("lorem ipsum dolor sit amet, consectetur adipiscing elit. ");
        }
        body.push_str(r#""}],"stream":true,"stream_options":{"include_usage":true}}"#);
        let body = body.into_bytes();
        assert!(body.len() >= (4 << 20));

        // Warm any lazily-initialised statics before measuring.
        let _ = peek(&body);

        let before = allocated();
        let p = peek(&body);
        let used = allocated() - before;

        assert!(used < 4096, "peek allocated {used} bytes on a 4 MiB body");
        assert_eq!(p.model.as_deref(), Some("carnice-9b"));
        assert!(p.stream);
        assert!(p.include_usage);
        assert_eq!(p.bytes, body.len());
    }

    #[test]
    fn normalize_path_collapses_a_duplicate_v1() {
        assert_eq!(
            normalize_path("/v1/v1/chat/completions"),
            ("/v1/chat/completions".to_owned(), true)
        );
        assert_eq!(
            normalize_path("/v1/v1/v1/models"),
            ("/v1/models".to_owned(), true)
        );
        assert_eq!(normalize_path("/v1/v1"), ("/v1".to_owned(), true));
    }

    #[test]
    fn normalize_path_leaves_everything_else_alone() {
        for path in [
            "/v1/chat/completions",
            "/v1/models",
            "/v1",
            "/v1beta/v1beta/models",
            "/v1/v1beta/models",
            "/v1/chat/v1/completions",
            "/health",
            "/",
            "",
        ] {
            assert_eq!(normalize_path(path), (path.to_owned(), false), "{path}");
        }
    }

    #[test]
    fn normalize_path_restores_the_v1_a_bare_base_url_omits() {
        // `docs/port/05-proxy.md` §15 item 8. A client configured with
        // `http://127.0.0.1:8888` sends these; every one must land on the same upstream
        // path as its `/v1`-prefixed twin.
        for (bare, want) in [
            ("/chat/completions", "/v1/chat/completions"),
            ("/completions", "/v1/completions"),
            ("/models", "/v1/models"),
            ("/models/auto", "/v1/models/auto"),
            ("/embeddings", "/v1/embeddings"),
            ("/rerank", "/v1/rerank"),
            ("/messages", "/v1/messages"),
            ("/messages/count_tokens", "/v1/messages/count_tokens"),
            ("/responses", "/v1/responses"),
            ("/audio/speech", "/v1/audio/speech"),
        ] {
            // Adding a missing prefix is NOT a collapse: this client is correct.
            assert_eq!(normalize_path(bare), (want.to_owned(), false), "{bare}");
            assert_eq!(normalize_path(want), (want.to_owned(), false), "{want}");
        }
    }

    #[test]
    fn normalize_path_never_versions_a_server_native_endpoint() {
        // llama.cpp's own surface, and `ARCHITECTURE.md` §6.1's "non-OpenAI paths
        // (`/props`, `/metrics`) forward raw". `/completion` and `/embedding` are the
        // singular llama.cpp natives and must NOT be confused with the OpenAI plurals.
        for path in [
            "/props",
            "/metrics",
            "/slots",
            "/health",
            "/providers",
            "/switch",
            "/completion",
            "/embedding",
            "/infill",
            "/tokenize",
            "/detokenize",
            "/apply-template",
            "/",
            "",
            "/modelsomething",
        ] {
            assert_eq!(normalize_path(path), (path.to_owned(), false), "{path}");
        }
    }

    /// Test-only sugar for the malformed-body assertion above.
    trait TapBytes {
        fn tap_bytes(self, n: usize) -> RequestPeek;
    }
    impl TapBytes for RequestPeek {
        fn tap_bytes(mut self, n: usize) -> RequestPeek {
            self.bytes = n;
            self
        }
    }
}
