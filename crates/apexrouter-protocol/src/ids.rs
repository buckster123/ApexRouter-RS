//! Validated newtypes. An empty or malformed id is **not constructible**, and
//! `Deserialize` goes through the same `parse()` every other caller uses — a bad id in a
//! state file is a load error, not a landmine that surfaces three subsystems later.
//!
//! Charset for every slug id: `^[a-z0-9][a-z0-9._-]{0,63}$`. `/` is deliberately outside it,
//! because `"<backend_id>/<upstream_model>"` is the explicit-pin syntax on the request path.

use std::fmt;

/// Maximum length of a slug id, in bytes. `^[a-z0-9][a-z0-9._-]{0,63}$` is 1..=64.
const MAX_SLUG_LEN: usize = 64;

/// Why an id failed to parse. Hand-written `Display`/`Error` because this crate deliberately
/// has no `thiserror` dependency — protocol is serde and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdError {
    /// The string was empty.
    Empty,
    /// The string exceeded 64 bytes.
    TooLong {
        /// The offending length.
        got: usize,
    },
    /// A character outside the charset, at a byte offset.
    BadChar {
        /// Byte offset of the offending character.
        at: usize,
        /// The offending character.
        ch: char,
    },
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IdError::Empty => write!(f, "id is empty"),
            IdError::TooLong { got } => {
                write!(f, "id is {got} bytes, the maximum is {MAX_SLUG_LEN}")
            }
            IdError::BadChar { at, ch } => write!(
                f,
                "id has an invalid character {ch:?} at byte {at}: \
                 allowed is ^[a-z0-9][a-z0-9._-]{{0,63}}$ ('/' is reserved for explicit pins)"
            ),
        }
    }
}

impl std::error::Error for IdError {}

/// Validate a slug against `^[a-z0-9][a-z0-9._-]{0,63}$`.
fn validate_slug(s: &str) -> Result<(), IdError> {
    if s.is_empty() {
        return Err(IdError::Empty);
    }
    if s.len() > MAX_SLUG_LEN {
        return Err(IdError::TooLong { got: s.len() });
    }
    for (at, ch) in s.char_indices() {
        let ok = if at == 0 {
            ch.is_ascii_lowercase() || ch.is_ascii_digit()
        } else {
            ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '.' || ch == '_' || ch == '-'
        };
        if !ok {
            return Err(IdError::BadChar { at, ch });
        }
    }
    Ok(())
}

macro_rules! slug_id {
    ($name:ident, $what:expr) => {
        #[doc = concat!("A validated ", $what, ".")]
        ///
        /// Charset `^[a-z0-9][a-z0-9._-]{0,63}$`. Construct with
        /// [`parse`](Self::parse) or `str::parse`; `Deserialize` rejects an invalid value.
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            #[doc = concat!("Parse and validate a ", $what, ".")]
            pub fn parse(s: &str) -> Result<Self, IdError> {
                validate_slug(s)?;
                Ok($name(s.to_owned()))
            }

            /// Borrow the validated string.
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Consume the id, yielding the validated string.
            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl std::str::FromStr for $name {
            type Err = IdError;
            fn from_str(s: &str) -> Result<Self, IdError> {
                $name::parse(s)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl std::borrow::Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.0)
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                $name::parse(&s).map_err(serde::de::Error::custom)
            }
        }
    };
}

slug_id!(BackendId, "backend id");
slug_id!(Alias, "route alias — the string a client puts in `model`");
slug_id!(BuildId, "llama.cpp build id, which is the build-dir name");
slug_id!(RecipeId, "recipe id");
slug_id!(ProfileId, "vast.ai search-profile id");
slug_id!(ProviderId, "managed-provider id");
slug_id!(
    ServiceId,
    "studio service id — a non-Backend lane on a rented box"
);

/// A vast.ai contract id. Note that the create call returns it as `new_contract`, not `id`.
#[derive(
    Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct InstanceId(pub u64);

impl fmt::Display for InstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Identifies one proxied request, end to end: the record, the log line and `X-Request-Id`.
#[derive(
    Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct RequestId(pub ulid::Ulid);

impl RequestId {
    /// Mint a fresh, monotonic-by-time id.
    pub fn new() -> Self {
        RequestId(ulid::Ulid::new())
    }
}

impl Default for RequestId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Identifies one background job behind `?no_wait=true`.
#[derive(
    Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct JobId(pub ulid::Ulid);

impl JobId {
    /// Mint a fresh, monotonic-by-time id.
    pub fn new() -> Self {
        JobId(ulid::Ulid::new())
    }
}

impl Default for JobId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_documented_charset() {
        for good in [
            "a",
            "0",
            "local-carnice",
            "vast.gguf_1",
            "z9",
            &"a".repeat(64),
        ] {
            assert!(BackendId::parse(good).is_ok(), "{good} should parse");
        }
    }

    #[test]
    fn rejects_empty_too_long_and_bad_chars() {
        assert_eq!(BackendId::parse(""), Err(IdError::Empty));
        assert_eq!(
            BackendId::parse(&"a".repeat(65)),
            Err(IdError::TooLong { got: 65 })
        );
        assert_eq!(
            BackendId::parse("-lead"),
            Err(IdError::BadChar { at: 0, ch: '-' })
        );
        assert_eq!(
            BackendId::parse("Upper"),
            Err(IdError::BadChar { at: 0, ch: 'U' })
        );
        assert_eq!(
            BackendId::parse("has space"),
            Err(IdError::BadChar { at: 3, ch: ' ' })
        );
    }

    #[test]
    fn alias_bans_the_explicit_pin_separator() {
        // '/' means "<backend_id>/<upstream_model>" on the request path, so it can never be
        // part of an alias.
        assert_eq!(
            Alias::parse("local/model"),
            Err(IdError::BadChar { at: 5, ch: '/' })
        );
    }

    #[test]
    fn deserialize_rejects_invalid_ids() {
        let ok: BackendId = serde_json::from_str("\"local-carnice\"").expect("valid id");
        assert_eq!(ok.as_str(), "local-carnice");
        assert!(serde_json::from_str::<BackendId>("\"Local Carnice\"").is_err());
        assert!(serde_json::from_str::<Alias>("\"\"").is_err());
    }

    #[test]
    fn slug_ids_round_trip_as_bare_strings() {
        let id = BackendId::parse("local-carnice").expect("valid");
        let s = serde_json::to_string(&id).expect("serialize");
        assert_eq!(s, "\"local-carnice\"");
        assert_eq!(serde_json::from_str::<BackendId>(&s).expect("parse"), id);
    }

    #[test]
    fn numeric_and_ulid_ids_round_trip() {
        let i = InstanceId(28_675_431);
        assert_eq!(serde_json::to_string(&i).expect("ser"), "28675431");
        assert_eq!(
            serde_json::from_str::<InstanceId>("28675431").expect("de"),
            i
        );

        let r = RequestId::new();
        let s = serde_json::to_string(&r).expect("ser");
        assert_eq!(serde_json::from_str::<RequestId>(&s).expect("de"), r);

        let j = JobId::new();
        let s = serde_json::to_string(&j).expect("ser");
        assert_eq!(serde_json::from_str::<JobId>(&s).expect("de"), j);
    }

    #[test]
    fn id_error_displays_without_panicking() {
        assert!(!IdError::Empty.to_string().is_empty());
        assert!(IdError::TooLong { got: 99 }.to_string().contains("99"));
        assert!(IdError::BadChar { at: 1, ch: '/' }
            .to_string()
            .contains('/'));
    }
}
