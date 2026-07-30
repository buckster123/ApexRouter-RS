//! OWNER: unit C-03 (core/secret.rs). Do not edit outside that unit.
//!
//! Secrets, and the **one** credential chain.
//!
//! [`Secret`] prints `***` in both `Debug` and `Display`; the sole accessor is
//! [`Secret::expose`]. Resolution order is one function with one order:
//! **explicit config value → ApexRouter config/`credentials.toml` → conventional
//! third-party path → the env var named by `api_key_env`.**
//!
//! Two structural rules, enforced here rather than by discipline:
//!
//! * A **borrowed** credential — one we merely read out of `$TOGETHER_API_KEY` or out of
//!   somebody else's config file — is *described* by a [`CredentialSource`] and never
//!   copied into anything we write. Nothing on the resolution path writes a file.
//! * [`store_user_credential`] is the only writer, it takes an owned [`Secret`], and the
//!   only legitimate caller is a surface where the **user typed the key** (a GUI field or
//!   `--key-stdin`).

use crate::config::Config;
use crate::error::{Error, Result};
use crate::paths::Paths;
use apexrouter_protocol::{CredentialSource, ProviderId};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

/// The name reported as the `Managed` store for a key held in `$STATE/credentials.toml`.
const CREDENTIAL_STORE: &str = "credentials.toml";

/// A value that must never be logged.
///
/// `Debug` and `Display` **both** print `***`, so a `{:?}` on any struct containing one is
/// safe by construction rather than by discipline.
#[derive(Clone)]
pub struct Secret<T>(T);

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl<T> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl<T> Secret<T> {
    /// Wrap any value.
    pub fn wrap(v: T) -> Self {
        Secret(v)
    }
}

impl Secret<String> {
    /// Wrap a key. Callers `.trim()` before wrapping — a trailing newline in a key file is
    /// the single most common cause of a 401 nobody can explain.
    pub fn new(s: String) -> Self {
        Secret(s)
    }

    /// The only way to read it. Every call site is grep-able.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

/// Where a credential may be found. The *inline* variant only ever holds a key the **user
/// typed**; a borrowed key is described, never copied.
#[derive(Clone, Debug)]
pub enum CredentialRef {
    /// An environment variable, by name.
    Env(String),
    /// A file, by path.
    File(PathBuf),
    /// A value the user typed into a GUI or `--key-stdin`.
    Inline(Secret<String>),
    /// Nothing configured.
    None,
}

/// A resolved credential, paired with the *description* of where it came from. The
/// description is what goes on the wire; the secret never does.
#[derive(Clone, Debug)]
pub struct ResolvedCredential {
    /// The key.
    pub secret: Secret<String>,
    /// Where it came from, for `GET /v1/providers`.
    pub source: CredentialSource,
}

/// The one chain, in the one order.
///
/// 1. `cfg_inline` — an explicit value: a key the user typed, held in
///    `$STATE/credentials.toml`. Reported as [`CredentialSource::Managed`].
/// 2. `cfg_file` — the key file **our** config names (`api_key_file`). Because the operator
///    asked for this file by name, an unreadable-but-present file is an error rather than a
///    silent fall-through; a merely *absent* one falls through.
/// 3. `conventional` — third-party paths in probe order (`~/.config/vastai/vast_api_key`,
///    `~/.cache/huggingface/token`, …). Unreadable ones are skipped, not fatal.
/// 4. `env_var` — the variable named by `api_key_env`.
///
/// Every read is `.trim()`ed, and a link that yields an empty string is treated as absent so
/// a stray blank key file cannot mask a working env var. Returns `Ok(None)` when no link
/// produced anything — that is a normal state, not an error.
///
/// This function never writes anything: a borrowed credential is not persisted.
pub fn resolve_credential(
    cfg_inline: Option<&Secret<String>>,
    cfg_file: Option<&Path>,
    conventional: &[&Path],
    env_var: Option<&str>,
) -> Result<Option<ResolvedCredential>> {
    // 1. explicit value.
    if let Some(s) = cfg_inline {
        let v = s.expose().trim();
        if !v.is_empty() {
            return Ok(Some(ResolvedCredential {
                secret: Secret::new(v.to_owned()),
                source: CredentialSource::Managed {
                    store: CREDENTIAL_STORE.to_owned(),
                },
            }));
        }
    }

    // 2. the key file our own config names.
    if let Some(p) = cfg_file {
        if let Some(v) = read_key_file(p, true)? {
            return Ok(Some(from_file(p, v)));
        }
    }

    // 3. conventional third-party paths.
    for p in conventional {
        if let Some(v) = read_key_file(p, false)? {
            return Ok(Some(from_file(p, v)));
        }
    }

    // 4. the environment.
    if let Some(var) = env_var {
        if let Some(v) = env_key(var) {
            return Ok(Some(ResolvedCredential {
                secret: Secret::new(v),
                source: CredentialSource::Env {
                    var: var.to_owned(),
                },
            }));
        }
    }

    Ok(None)
}

/// vast.ai's credential, through the chain.
///
/// `credentials.toml` → `[vast] api_key_file` → `~/.config/vastai/vast_api_key` (the
/// `vastai` CLI's own convention) → `$VAST_API_KEY`, then `$VASTAI_API_KEY`.
pub fn resolve_vast(cfg: &Config, paths: &Paths) -> Result<Option<ResolvedCredential>> {
    let inline = stored_credential(&paths.credentials_file(), "vast")?;
    let cfg_file = non_empty_path(&cfg.vast.api_key_file);
    let conventional = paths.legacy().vast_key.clone();

    let found = resolve_credential(
        inline.as_ref(),
        cfg_file.as_deref(),
        &[conventional.as_path()],
        Some("VAST_API_KEY"),
    )?;
    if found.is_some() {
        return Ok(found);
    }
    resolve_credential(None, None, &[], Some("VASTAI_API_KEY"))
}

/// HuggingFace's token, through the chain.
///
/// `credentials.toml` → `[hf] token_file` → `~/.cache/huggingface/token` → `$HF_TOKEN`,
/// then `$HUGGING_FACE_HUB_TOKEN` (the `huggingface_hub` library's own variable).
pub fn resolve_hf(cfg: &Config, paths: &Paths) -> Result<Option<ResolvedCredential>> {
    let inline = stored_credential(&paths.credentials_file(), "hf")?;
    let cfg_file = non_empty_path(&cfg.hf.token_file);
    let conventional = paths.legacy().hf_token.clone();

    let found = resolve_credential(
        inline.as_ref(),
        cfg_file.as_deref(),
        &[conventional.as_path()],
        Some("HF_TOKEN"),
    )?;
    if found.is_some() {
        return Ok(found);
    }
    resolve_credential(None, None, &[], Some("HUGGING_FACE_HUB_TOKEN"))
}

/// A managed provider's key, through the chain, including `~/.vastai-gguf/config.toml`.
///
/// `credentials.toml` → `[providers.<id>] api_key_file` → `~/.vastai-gguf/config.toml`
/// (step 3, gated by `[compat] read_legacy_state`, parsed with a **real TOML parser**) →
/// the env var named by `api_key_env`, defaulting to `<ID>_API_KEY`.
///
/// The legacy file is **never rewritten** and the `base_url` it carries is used verbatim:
/// `api.together.xyz` must not become `api.together.ai`.
pub fn resolve_provider(
    cfg: &Config,
    paths: &Paths,
    id: &ProviderId,
) -> Result<Option<ResolvedCredential>> {
    let pcfg = cfg.provider(id);
    let inline = stored_credential(&paths.credentials_file(), id.as_str())?;
    let cfg_file = pcfg
        .and_then(|p| p.api_key_file.as_deref())
        .and_then(non_empty_path);

    // Links 1 and 2.
    if let Some(found) = resolve_credential(inline.as_ref(), cfg_file.as_deref(), &[], None)? {
        return Ok(Some(found));
    }

    // Link 3: the conventional third-party path for a managed provider is LocalRouter's
    // own config, which is a TOML document rather than a bare key file.
    if cfg.compat.read_legacy_state {
        let legacy = paths.legacy().vastai_gguf.join("config.toml");
        if let Some(entry) = legacy_provider(&legacy, id.as_str()) {
            if let Some(key) = entry.api_key.as_deref().map(str::trim) {
                if !key.is_empty() {
                    return Ok(Some(from_file(&legacy, key.to_owned())));
                }
            }
        }
    }

    // Link 4.
    let env_var = pcfg
        .and_then(|p| p.api_key_env.clone())
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default_env_var(id.as_str()));
    resolve_credential(None, None, &[], Some(env_var.as_str()))
}

/// Only ever called with a key the USER typed. Writes `$STATE/credentials.toml` at `0600`.
///
/// A borrowed key — one we merely read out of `$TOGETHER_API_KEY` or somebody else's config
/// — must never reach this function. Nothing on the [`resolve_credential`] path calls it.
///
/// The document is round-tripped through `toml_edit`, so hand-written comments and any other
/// provider already in the file survive. The write is atomic (tmp in the same directory →
/// `fsync` → `rename` → `fsync(dir)`) and the mode is set at `OpenOptions` time.
pub fn store_user_credential(paths: &Paths, id: &ProviderId, key: Secret<String>) -> Result<()> {
    put_credential(&paths.credentials_file(), id.as_str(), key.expose())
}

// ---------------------------------------------------------------------------------------
// internals
// ---------------------------------------------------------------------------------------

/// One `[providers.<id>]` table as the legacy `~/.vastai-gguf/config.toml` writes it.
#[derive(Debug, Default, Deserialize)]
struct LegacyProvider {
    /// Used verbatim by callers; never rewritten.
    base_url: Option<String>,
    api_key: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ProviderTables {
    #[serde(default)]
    providers: BTreeMap<String, LegacyProvider>,
}

/// Read `path` as a key file: trim, and treat empty as absent.
///
/// `strict` distinguishes a path the operator named in our config (a read failure other than
/// "not found" is an error they need to see) from a conventional probe path (skipped).
fn read_key_file(path: &Path, strict: bool) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(s) => {
            let t = s.trim();
            if t.is_empty() {
                tracing::debug!(path = %path.display(), "credential file is empty");
                Ok(None)
            } else {
                Ok(Some(t.to_owned()))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => {
            if strict {
                Err(Error::Io {
                    path: path.display().to_string(),
                    source: e,
                })
            } else {
                tracing::debug!(path = %path.display(), error = %e, "skipping credential path");
                Ok(None)
            }
        }
    }
}

/// A resolved credential that came out of a file.
fn from_file(path: &Path, value: String) -> ResolvedCredential {
    ResolvedCredential {
        secret: Secret::new(value),
        source: CredentialSource::File {
            path: path.display().to_string(),
        },
    }
}

/// Read an env var, trimmed; empty or unset is absent.
fn env_key(var: &str) -> Option<String> {
    match std::env::var(var) {
        Ok(v) => {
            let t = v.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_owned())
            }
        }
        Err(_) => None,
    }
}

/// `together` → `TOGETHER_API_KEY`. Used when `api_key_env` is not configured.
fn default_env_var(id: &str) -> String {
    let mut s: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    s.push_str("_API_KEY");
    s
}

/// Expand a leading `~` and reject the empty string.
fn non_empty_path(s: &str) -> Option<PathBuf> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    Some(expand_tilde(s))
}

/// `~` and `~/…` against `$HOME`. Any other form is returned unchanged.
fn expand_tilde(s: &str) -> PathBuf {
    if s == "~" {
        if let Some(h) = dirs::home_dir() {
            return h;
        }
    } else if let Some(rest) = s.strip_prefix("~/") {
        if let Some(h) = dirs::home_dir() {
            return h.join(rest);
        }
    }
    PathBuf::from(s)
}

/// Parse `~/.vastai-gguf/config.toml` with a real TOML parser and return one provider's
/// entry. A malformed third-party file is *skipped*, not fatal — it is not ours to fix.
fn legacy_provider(path: &Path, id: &str) -> Option<LegacyProvider> {
    let text = fs::read_to_string(path).ok()?;
    match toml::from_str::<ProviderTables>(&text) {
        Ok(mut t) => t.providers.remove(id),
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "legacy provider config unparseable");
            None
        }
    }
}

/// Read one key out of `$STATE/credentials.toml`. This file **is** ours, so a parse error is
/// reported rather than swallowed.
fn stored_credential(path: &Path, id: &str) -> Result<Option<Secret<String>>> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(Error::Io {
                path: path.display().to_string(),
                source: e,
            })
        }
    };
    let mut tables: ProviderTables = toml::from_str(&text)?;
    let key = tables
        .providers
        .remove(id)
        .and_then(|p| p.api_key)
        .map(|k| k.trim().to_owned())
        .filter(|k| !k.is_empty());
    Ok(key.map(Secret::new))
}

/// `toml_edit` round-trip + atomic `0600` write of one `[providers.<id>] api_key`.
fn put_credential(file: &Path, id: &str, key: &str) -> Result<()> {
    let key = key.trim();
    if key.is_empty() {
        return Err(Error::Invalid {
            what: "credential".to_owned(),
            why: "an empty key is not a credential".to_owned(),
        });
    }

    let mut doc = match fs::read_to_string(file) {
        Ok(t) => t
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| Error::Invalid {
                what: file.display().to_string(),
                why: e.to_string(),
            })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => toml_edit::DocumentMut::new(),
        Err(e) => {
            return Err(Error::Io {
                path: file.display().to_string(),
                source: e,
            })
        }
    };

    let providers = doc
        .entry("providers")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let providers = providers.as_table_mut().ok_or_else(|| Error::Invalid {
        what: file.display().to_string(),
        why: "`providers` is not a table".to_owned(),
    })?;
    providers.set_implicit(true);
    let entry = providers
        .entry(id)
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let entry = entry.as_table_mut().ok_or_else(|| Error::Invalid {
        what: file.display().to_string(),
        why: format!("`providers.{id}` is not a table"),
    })?;
    entry["api_key"] = toml_edit::value(key);

    write_atomic_0600(file, doc.to_string().as_bytes())
}

/// tmp in the SAME dir → write → `fsync(file)` → `rename` → `fsync(dir)`. Mode is set at
/// `OpenOptions` time so the key is never briefly world-readable.
fn write_atomic_0600(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().ok_or_else(|| Error::Invalid {
        what: path.display().to_string(),
        why: "path has no parent directory".to_owned(),
    })?;
    if !dir.as_os_str().is_empty() && !dir.exists() {
        fs::create_dir_all(dir).map_err(|e| Error::Io {
            path: dir.display().to_string(),
            source: e,
        })?;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }

    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("credentials.toml");
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));

    let io = |p: &Path, e: std::io::Error| Error::Io {
        path: p.display().to_string(),
        source: e,
    };

    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| io(&tmp, e))?;
        // The mode above applies only on creation; a leftover tmp must not keep old bits.
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600)).map_err(|e| io(&tmp, e))?;
        f.write_all(bytes).map_err(|e| io(&tmp, e))?;
        f.sync_all().map_err(|e| io(&tmp, e))?;
    }

    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        io(path, e)
    })?;

    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Unique env var names, because the environment is process-global and tests run in
    /// parallel threads.
    fn unique_env(prefix: &str) -> String {
        static N: AtomicU32 = AtomicU32::new(0);
        format!(
            "APEXROUTER_TEST_{prefix}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn write(path: &Path, body: &str) {
        if let Some(d) = path.parent() {
            fs::create_dir_all(d).expect("mkdir");
        }
        fs::write(path, body).expect("write");
    }

    #[test]
    fn secret_never_prints_its_contents() {
        let s = Secret::new("sk-live-DO-NOT-LOG-THIS".to_owned());
        assert_eq!(format!("{s:?}"), "***");
        assert_eq!(format!("{s}"), "***");
        assert!(!format!("{s:?} {s}").contains("sk-live"));
        assert_eq!(s.expose(), "sk-live-DO-NOT-LOG-THIS");
    }

    #[test]
    fn a_struct_containing_a_secret_is_safe_to_debug() {
        #[derive(Debug)]
        struct Holder {
            key: Secret<String>,
        }
        let h = Holder {
            key: Secret::new("sk-live-DO-NOT-LOG-THIS".to_owned()),
        };
        assert!(!format!("{h:?}").contains("sk-live"), "{h:?}");
    }

    #[test]
    fn a_resolved_credential_is_safe_to_debug() {
        let r = ResolvedCredential {
            secret: Secret::new("sk-live-DO-NOT-LOG-THIS".to_owned()),
            source: CredentialSource::Env {
                var: "TOGETHER_API_KEY".to_owned(),
            },
        };
        let rendered = format!("{r:?}");
        assert!(!rendered.contains("sk-live"), "{rendered}");
        assert!(rendered.contains("TOGETHER_API_KEY"), "{rendered}");
    }

    /// The four-case order test: explicit → config file → conventional → env var.
    #[test]
    fn the_chain_order_is_explicit_then_file_then_conventional_then_env() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_file = dir.path().join("from-config.key");
        let conv = dir.path().join("conventional.key");
        write(&cfg_file, "key-from-config-file\n");
        write(&conv, "key-from-conventional\n");
        let var = unique_env("ORDER");
        std::env::set_var(&var, "key-from-env");

        let inline = Secret::new("key-from-explicit".to_owned());

        // 1. all four present -> explicit wins.
        let r = resolve_credential(
            Some(&inline),
            Some(&cfg_file),
            &[conv.as_path()],
            Some(var.as_str()),
        )
        .expect("resolve")
        .expect("some");
        assert_eq!(r.secret.expose(), "key-from-explicit");
        assert_eq!(
            r.source,
            CredentialSource::Managed {
                store: "credentials.toml".to_owned()
            }
        );

        // 2. no explicit -> the config file wins.
        let r = resolve_credential(None, Some(&cfg_file), &[conv.as_path()], Some(var.as_str()))
            .expect("resolve")
            .expect("some");
        assert_eq!(r.secret.expose(), "key-from-config-file");
        assert_eq!(
            r.source,
            CredentialSource::File {
                path: cfg_file.display().to_string()
            }
        );

        // 3. no config file -> the conventional path wins.
        let r = resolve_credential(None, None, &[conv.as_path()], Some(var.as_str()))
            .expect("resolve")
            .expect("some");
        assert_eq!(r.secret.expose(), "key-from-conventional");
        assert_eq!(
            r.source,
            CredentialSource::File {
                path: conv.display().to_string()
            }
        );

        // 4. nothing on disk -> the env var.
        let r = resolve_credential(None, None, &[], Some(var.as_str()))
            .expect("resolve")
            .expect("some");
        assert_eq!(r.secret.expose(), "key-from-env");
        assert_eq!(r.source, CredentialSource::Env { var: var.clone() });

        // 5. no link at all is Ok(None), never an error.
        std::env::remove_var(&var);
        assert!(resolve_credential(None, None, &[], Some(var.as_str()))
            .expect("resolve")
            .is_none());
    }

    #[test]
    fn conventional_paths_are_probed_in_order_and_missing_ones_are_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nope.key");
        let second = dir.path().join("second.key");
        let third = dir.path().join("third.key");
        write(&second, "second");
        write(&third, "third");

        let r = resolve_credential(
            None,
            None,
            &[missing.as_path(), second.as_path(), third.as_path()],
            None,
        )
        .expect("resolve")
        .expect("some");
        assert_eq!(r.secret.expose(), "second");
    }

    #[test]
    fn every_read_is_trimmed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("k");
        write(&f, "\n  sk-trimmed  \n\n");
        let r = resolve_credential(None, Some(&f), &[], None)
            .expect("resolve")
            .expect("some");
        assert_eq!(r.secret.expose(), "sk-trimmed");

        let var = unique_env("TRIM");
        std::env::set_var(&var, "  sk-env-trimmed \n");
        let r = resolve_credential(None, None, &[], Some(var.as_str()))
            .expect("resolve")
            .expect("some");
        assert_eq!(r.secret.expose(), "sk-env-trimmed");
        std::env::remove_var(&var);

        let inline = Secret::new(" sk-inline-trimmed\n".to_owned());
        let r = resolve_credential(Some(&inline), None, &[], None)
            .expect("resolve")
            .expect("some");
        assert_eq!(r.secret.expose(), "sk-inline-trimmed");
    }

    #[test]
    fn an_empty_link_falls_through_instead_of_masking_the_next_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let blank = dir.path().join("blank.key");
        write(&blank, "   \n\t\n");
        let var = unique_env("BLANK");
        std::env::set_var(&var, "the-real-key");

        let empty_inline = Secret::new("   ".to_owned());
        let r = resolve_credential(Some(&empty_inline), Some(&blank), &[], Some(var.as_str()))
            .expect("resolve")
            .expect("some");
        assert_eq!(r.secret.expose(), "the-real-key");
        std::env::remove_var(&var);
    }

    #[test]
    fn an_unreadable_configured_file_is_an_error_but_a_conventional_one_is_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("locked.key");
        write(&f, "secret");
        fs::set_permissions(&f, fs::Permissions::from_mode(0o000)).expect("chmod");

        // Running as root defeats mode 0o000; skip rather than assert a false negative.
        if fs::read_to_string(&f).is_ok() {
            return;
        }

        assert!(resolve_credential(None, Some(&f), &[], None).is_err());
        assert!(resolve_credential(None, None, &[f.as_path()], None)
            .expect("conventional paths are skipped, not fatal")
            .is_none());
    }

    #[test]
    fn the_legacy_config_is_parsed_with_toml_and_its_base_url_is_returned_unmodified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("config.toml");
        write(
            &f,
            r#"# ~/.vastai-gguf/config.toml — provider configuration
# api_key = "sk-this-is-a-comment-not-a-key"

[providers.together]
base_url  = "https://api.together.xyz/v1"
api_key   = "sk-together-real\t"

[providers.other]
base_url = "https://example.invalid"
api_key  = "sk-other"
"#,
        );

        let e = legacy_provider(&f, "together").expect("entry");
        // Never rewritten: `.xyz` must not become `.ai`.
        assert_eq!(e.base_url.as_deref(), Some("https://api.together.xyz/v1"));
        assert_eq!(
            e.api_key.as_deref().map(str::trim),
            Some("sk-together-real")
        );

        // Real parsing, not string matching: the commented-out line is not a hit, and the
        // right section is picked out of several.
        let other = legacy_provider(&f, "other").expect("entry");
        assert_eq!(other.api_key.as_deref(), Some("sk-other"));
        assert!(legacy_provider(&f, "missing").is_none());
    }

    #[test]
    fn a_malformed_legacy_config_is_skipped_not_fatal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("config.toml");
        write(&f, "this is not = = toml [[[");
        assert!(legacy_provider(&f, "together").is_none());
    }

    #[test]
    fn stored_credentials_round_trip_and_preserve_comments_and_siblings() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("state").join("credentials.toml");

        // A file that does not exist yet is created, with its parent.
        put_credential(&f, "together", "  sk-typed-by-the-user \n").expect("put");
        assert_eq!(
            stored_credential(&f, "together")
                .expect("read")
                .expect("some")
                .expose(),
            "sk-typed-by-the-user"
        );

        // A hand-written comment survives, and so does a sibling provider.
        let body = fs::read_to_string(&f).expect("read");
        write(&f, &format!("# hand written\n{body}"));
        put_credential(&f, "deepinfra", "sk-second").expect("put");

        let body = fs::read_to_string(&f).expect("read");
        assert!(body.contains("# hand written"), "{body}");
        assert_eq!(
            stored_credential(&f, "together")
                .expect("read")
                .expect("some")
                .expose(),
            "sk-typed-by-the-user"
        );
        assert_eq!(
            stored_credential(&f, "deepinfra")
                .expect("read")
                .expect("some")
                .expose(),
            "sk-second"
        );

        // Overwriting one key leaves the other alone.
        put_credential(&f, "together", "sk-rotated").expect("put");
        assert_eq!(
            stored_credential(&f, "together")
                .expect("read")
                .expect("some")
                .expose(),
            "sk-rotated"
        );
        assert_eq!(
            stored_credential(&f, "deepinfra")
                .expect("read")
                .expect("some")
                .expose(),
            "sk-second"
        );
    }

    #[test]
    fn the_credential_store_is_0600_and_its_directory_0700() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("state").join("credentials.toml");
        put_credential(&f, "together", "sk-typed").expect("put");

        let mode = fs::metadata(&f).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "credentials.toml is {mode:o}");
        let dmode = fs::metadata(f.parent().expect("parent"))
            .expect("stat")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dmode, 0o700, "state dir is {dmode:o}");

        // No temp file is left behind.
        let leftovers: Vec<_> = fs::read_dir(f.parent().expect("parent"))
            .expect("readdir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn an_empty_key_is_never_stored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("credentials.toml");
        assert!(put_credential(&f, "together", "   \n").is_err());
        assert!(!f.exists());
    }

    /// A **borrowed** key never reaches the store: resolving writes nothing, ever.
    #[test]
    fn resolution_never_writes_a_borrowed_key_into_the_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = dir.path().join("state");
        fs::create_dir_all(&state).expect("mkdir");
        let store = state.join("credentials.toml");

        let borrowed_file = dir.path().join("someone-elses.key");
        write(&borrowed_file, "sk-borrowed-from-a-third-party");
        let var = unique_env("BORROWED");
        std::env::set_var(&var, "sk-borrowed-from-the-env");

        let r = resolve_credential(None, None, &[borrowed_file.as_path()], Some(var.as_str()))
            .expect("resolve")
            .expect("some");
        assert_eq!(r.secret.expose(), "sk-borrowed-from-a-third-party");
        let r = resolve_credential(None, None, &[], Some(var.as_str()))
            .expect("resolve")
            .expect("some");
        assert_eq!(r.secret.expose(), "sk-borrowed-from-the-env");
        std::env::remove_var(&var);

        assert!(
            !store.exists(),
            "the credential store must not be created by resolution"
        );
        assert!(stored_credential(&store, "together")
            .expect("read")
            .is_none());
    }

    #[test]
    fn a_bad_store_is_reported_rather_than_swallowed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("credentials.toml");
        write(&f, "[providers.together\napi_key = ");
        assert!(stored_credential(&f, "together").is_err());
    }

    #[test]
    fn the_default_env_var_is_derived_from_the_provider_id() {
        assert_eq!(default_env_var("together"), "TOGETHER_API_KEY");
        assert_eq!(default_env_var("vast-gguf"), "VAST_GGUF_API_KEY");
        assert_eq!(default_env_var("deep.infra"), "DEEP_INFRA_API_KEY");
    }

    #[test]
    fn tilde_expands_against_home_and_nothing_else_is_touched() {
        let home = dirs::home_dir().expect("home");
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(
            expand_tilde("~/.config/vastai"),
            home.join(".config/vastai")
        );
        assert_eq!(expand_tilde("/etc/keys/x"), PathBuf::from("/etc/keys/x"));
        assert_eq!(expand_tilde("~other/x"), PathBuf::from("~other/x"));
        assert!(non_empty_path("   ").is_none());
    }
}
