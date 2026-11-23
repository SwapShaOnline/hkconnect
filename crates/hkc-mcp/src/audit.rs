//! Append-only audit log of remote commands, one JSON object per line.
//!
//! Each line records when, which tool, its arguments, which OAuth client
//! called it (or `"stdio"`), and the outcome. Tool arguments never contain
//! secrets (the Bluelink password and PIN come from the environment, and
//! bearer tokens are never passed to tools), so they are logged as given.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use jiff::Timestamp;
use serde::Serialize;
use serde_json::Value;

/// One audit record.
#[derive(Debug, Serialize)]
pub struct Entry<'a> {
    /// When the call finished (UTC).
    pub time: Timestamp,
    /// Tool name.
    pub tool: &'a str,
    /// Tool arguments as received.
    pub arguments: &'a Value,
    /// The OAuth client id, or `"stdio"`.
    pub client: &'a str,
    /// The car, by VIN, when one was resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vin: Option<&'a str>,
    /// The command id, if the server accepted the command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command_id: Option<&'a str>,
    /// Outcome: `succeeded`, `failed`, `pending`, `not_sent`, `error`, ...
    pub outcome: &'a str,
    /// Error text, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<&'a str>,
}

/// The audit log file. Writes are serialized; a write failure is logged to
/// stderr but never fails the tool call (the command has already been sent).
#[derive(Debug)]
pub struct AuditLog {
    path: Option<PathBuf>,
    lock: Mutex<()>,
}

impl AuditLog {
    /// Log to `path` (created 0600 if missing).
    pub fn new(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            lock: Mutex::new(()),
        }
    }

    /// A log that discards everything (for tests that don't check it).
    #[cfg(test)]
    pub fn disabled() -> Self {
        Self {
            path: None,
            lock: Mutex::new(()),
        }
    }

    /// Append one record.
    pub fn record(&self, entry: &Entry<'_>) {
        let Some(path) = &self.path else { return };
        let mut line = match serde_json::to_string(entry) {
            Ok(line) => line,
            Err(e) => {
                tracing::error!(error = %e, "could not serialize audit entry");
                return;
            }
        };
        line.push('\n');
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Err(e) = append(path, line.as_bytes()) {
            tracing::error!(error = %e, path = %path.display(), "could not write audit log");
        }
    }
}

fn append(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        crate::store::create_private_dir(dir)?;
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    // One write per line keeps lines whole even if another process appends.
    file.write_all(bytes)?;
    file.sync_data()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_json_lines_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("audit.jsonl");
        let log = AuditLog::new(path.clone());
        let args = serde_json::json!({ "vehicle": "Creta" });
        for outcome in ["succeeded", "failed"] {
            log.record(&Entry {
                time: Timestamp::now(),
                tool: "lock",
                arguments: &args,
                client: "stdio",
                vin: Some("VIN1"),
                command_id: Some("cmd-1"),
                outcome,
                error: None,
            });
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["tool"], "lock");
        assert_eq!(lines[1]["outcome"], "failed");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
