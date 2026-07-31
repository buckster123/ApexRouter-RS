//! Talking to a fake that is running in **another process** — one the supervisor started.
//!
//! Everything here is blocking and dependency-free, so it works from a `#[test]` as well
//! as from a `#[tokio::test]`, and needs no client of its own.
//!
//! ```no_run
//! # use apexrouter_tests_support::Control;
//! let fake = Control::at("http://127.0.0.1:8100");
//! let argv = fake.record().expect("the launch record");     // what it was exec'd with
//! assert_eq!(argv.flag("-c"), Some("32768"));
//! fake.set_behavior("chat_status=503").expect("make it fail");
//! ```

use crate::http;
use crate::record::LaunchRecord;
use crate::server::RecordedRequest;

/// A handle to a fake `llama-server` reachable over loopback.
#[derive(Clone, Debug)]
pub struct Control {
    base_url: String,
}

impl Control {
    /// Point at a running fake. `base_url` is the stored form, without `/v1`.
    pub fn at(base_url: &str) -> Control {
        Control {
            base_url: base_url.trim_end_matches('/').to_owned(),
        }
    }

    /// The base URL this control talks to.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// What the fake was launched with — the same [`LaunchRecord`] it wrote to disk,
    /// fetched over HTTP so a test does not have to know where the records directory is.
    ///
    /// # Errors
    /// Transport failure, or a non-2xx status.
    pub fn record(&self) -> Result<LaunchRecord, String> {
        let v = http::request(&self.base_url, "GET", "/_apex/record", None)?;
        serde_json::from_value(v).map_err(|e| format!("/_apex/record: {e}"))
    }

    /// Every request the fake has received, oldest first.
    ///
    /// # Errors
    /// Transport failure, or a non-2xx status.
    pub fn requests(&self) -> Result<Vec<RecordedRequest>, String> {
        let v = http::request(&self.base_url, "GET", "/_apex/requests", None)?;
        serde_json::from_value(v).map_err(|e| format!("/_apex/requests: {e}"))
    }

    /// Forget the recorded requests.
    ///
    /// # Errors
    /// Transport failure, or a non-2xx status.
    pub fn clear_requests(&self) -> Result<(), String> {
        http::request(&self.base_url, "DELETE", "/_apex/requests", None).map(|_| ())
    }

    /// Change its behaviour, live. Same spec syntax as everywhere else.
    ///
    /// # Errors
    /// Transport failure, or a non-2xx status.
    pub fn set_behavior(&self, spec: &str) -> Result<(), String> {
        let body = serde_json::Value::String(spec.to_owned());
        // A JSON string body is applied as a spec; an object is applied key by key.
        http::request(&self.base_url, "POST", "/_apex/behavior", Some(&body)).map(|_| ())?;
        Ok(())
    }

    /// Make it exit with `code`, so a test can watch the supervisor notice.
    ///
    /// The response races the exit; a transport error here means it went even faster than
    /// usual, which is why the result is discarded.
    pub fn exit(&self, code: i32) {
        let path = format!("/_apex/exit?code={code}");
        let _ = http::request(&self.base_url, "POST", &path, None);
    }
}
