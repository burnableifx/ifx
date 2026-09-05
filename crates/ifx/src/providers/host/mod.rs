//! Host-scoped resources (files, packages, services, commands, users) executed through a
//! [`crate::transport::Transport`]. Every type takes an `on` connection input and works
//! the same way locally and over SSH.

mod exec;
mod file;
mod package;
mod service;
mod user;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::provider::{Ctx, Registry, Result};
use crate::schema::{FieldSchema, FieldType, field};
use crate::transport::Transport;

pub use crate::generated::host::*;
pub use exec::ExecHandler;
pub use file::FileHandler;
pub use package::{Manager, PackageHandler};
pub use service::ServiceHandler;
pub use user::UserHandler;

pub fn register(r: &mut Registry) {
    r.register(FileHandler);
    r.register(PackageHandler::default());
    r.register(ServiceHandler);
    r.register(ExecHandler);
    r.register(UserHandler);
}

/// The `on` input shared by every host resource.
pub(crate) fn on_field() -> FieldSchema {
    field("on", FieldType::Connection).required().replace().doc(
        "Connection to the target host: `{\"kind\": \"local\"}` or an SSH connection, usually an \
         instance's `connection` output. Changing it replaces the resource on the new host.",
    )
}

pub(crate) async fn transport(cx: &Ctx<'_>, inputs: &Value) -> Result<Arc<dyn Transport>> {
    cx.transports.from_value(&inputs["on"]).await
}

pub(crate) fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

pub(crate) fn req_str<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    str_field(v, key).ok_or_else(|| anyhow::anyhow!("missing required input `{key}`"))
}

pub(crate) fn bool_field(v: &Value, key: &str) -> Option<bool> {
    v.get(key).and_then(Value::as_bool)
}

/// A list-of-strings input; absent or malformed items are dropped.
pub(crate) fn str_list(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Sorted, de-duplicated copy of a string list (for set-valued fields).
pub(crate) fn sorted_set(items: &[String]) -> Vec<String> {
    let mut v: Vec<String> = items.iter().map(|s| s.trim().to_string()).collect();
    v.sort();
    v.dedup();
    v
}

/// Single-quote a string for `sh`.
pub(crate) fn esc(s: &str) -> String {
    shell_escape::unix::escape(s.into()).into_owned()
}

pub(crate) fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(data))
}

/// Current UTC time as RFC 3339 (`2026-08-27T12:34:56Z`).
pub(crate) fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    rfc3339(secs)
}

pub(crate) fn rfc3339(unix_secs: u64) -> String {
    let (y, m, d) = civil_from_days((unix_secs / 86400) as i64);
    let rem = unix_secs % 86400;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_dates() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn helpers() {
        let v = serde_json::json!({"a": "x", "b": true, "l": ["b", "a", 1, "a"]});
        assert_eq!(str_field(&v, "a"), Some("x"));
        assert!(req_str(&v, "zz").is_err());
        assert_eq!(bool_field(&v, "b"), Some(true));
        assert_eq!(str_list(&v, "l"), ["b", "a", "a"]);
        assert_eq!(sorted_set(&str_list(&v, "l")), ["a", "b"]);
        assert_eq!(esc("it's"), "'it'\\''s'");
        assert_eq!(sha256_hex(b"").len(), 64);
    }
}
