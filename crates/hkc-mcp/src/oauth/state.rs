//! The OAuth server's persisted state: the password hash, registered (DCR)
//! clients, and issued tokens, kept only as SHA-256 hashes.
//!
//! The file is re-read on every operation and rewritten atomically (0600)
//! after every change, so `hkc-mcp revoke-all` or `set-password` run from
//! another process take effect on the running server's next request.
//!
//! Every read-modify-write holds an exclusive advisory lock on a lock file
//! next to the state file (`<name>.lock`), taken by the server and the CLI
//! subcommands alike, so a CLI change can't be lost to a server write that
//! started from an older copy. An update that changes nothing does not
//! rewrite the file.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::crypto::{hash_token, random_token};
use crate::store::write_private;

/// Format version written by this release.
const VERSION: u32 = 1;
/// Longest wait for another process's lock on the state file.
const LOCK_WAIT: Duration = Duration::from_secs(10);

/// Everything persisted.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// File format version.
    #[serde(default)]
    pub version: u32,
    /// Argon2id PHC string of the sign-in password.
    #[serde(default)]
    pub password_hash: Option<String>,
    /// Clients registered with dynamic client registration, by client id.
    #[serde(default)]
    pub clients: BTreeMap<String, RegisteredClient>,
    /// Issued tokens, keyed by the SHA-256 of the token.
    #[serde(default)]
    pub tokens: BTreeMap<String, TokenRecord>,
}

/// A client registered through `/register`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredClient {
    /// Allowed redirect URIs, matched exactly.
    pub redirect_uris: Vec<String>,
    /// Display name the client gave.
    #[serde(default)]
    pub client_name: Option<String>,
    /// When it registered.
    pub registered_at: Timestamp,
}

/// Access or refresh token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Bearer token for `/mcp`.
    Access,
    /// Single-use token for getting a new pair.
    Refresh,
}

/// One issued token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenRecord {
    /// Access or refresh.
    pub kind: Kind,
    /// All tokens descended from one authorization share a family; reuse
    /// of a rotated refresh token revokes the whole family.
    pub family: String,
    /// The client it was issued to.
    pub client_id: String,
    /// The protected resource (audience) it is valid for.
    pub resource: String,
    /// Expiry.
    pub expires_at: Timestamp,
    /// For refresh tokens: already exchanged (kept until expiry to detect
    /// reuse).
    #[serde(default)]
    pub used: bool,
}

/// A freshly issued token pair (plain text, shown to the client once).
#[derive(Debug)]
pub struct IssuedTokens {
    /// The access token.
    pub access_token: String,
    /// The refresh token.
    pub refresh_token: String,
}

impl State {
    /// Issue an access and a refresh token in `family`.
    pub fn issue(
        &mut self,
        family: &str,
        client_id: &str,
        resource: &str,
        access_ttl: SignedDuration,
        refresh_ttl: SignedDuration,
    ) -> IssuedTokens {
        let now = Timestamp::now();
        let access_token = random_token();
        let refresh_token = random_token();
        for (token, kind, ttl) in [
            (&access_token, Kind::Access, access_ttl),
            (&refresh_token, Kind::Refresh, refresh_ttl),
        ] {
            self.tokens.insert(
                hash_token(token),
                TokenRecord {
                    kind,
                    family: family.to_owned(),
                    client_id: client_id.to_owned(),
                    resource: resource.to_owned(),
                    expires_at: now.saturating_add(ttl).unwrap_or(now),
                    used: false,
                },
            );
        }
        IssuedTokens {
            access_token,
            refresh_token,
        }
    }

    /// Drop every token of a family. Returns how many were removed.
    pub fn revoke_family(&mut self, family: &str) -> usize {
        let before = self.tokens.len();
        self.tokens.retain(|_, t| t.family != family);
        before - self.tokens.len()
    }

    /// The valid (unexpired) access token with this plain value.
    pub fn access(&self, token: &str, now: Timestamp) -> Option<&TokenRecord> {
        self.tokens
            .get(&hash_token(token))
            .filter(|t| t.kind == Kind::Access && t.expires_at > now)
    }

    fn prune(&mut self, now: Timestamp) {
        self.tokens.retain(|_, t| t.expires_at > now);
    }
}

/// The state file, with in-process and cross-process serialization of
/// read-modify-write.
#[derive(Debug)]
pub struct StateFile {
    path: PathBuf,
    lock: Mutex<()>,
}

/// Holds the exclusive file lock until dropped.
struct FileLock(File);

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

impl StateFile {
    /// The state kept at `path`.
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    fn lock_path(&self) -> PathBuf {
        let mut name = self
            .path
            .file_name()
            .map(std::ffi::OsStr::to_os_string)
            .unwrap_or_default();
        name.push(".lock");
        self.path.with_file_name(name)
    }

    /// Take the exclusive cross-process lock, waiting (without blocking the
    /// runtime) up to [`LOCK_WAIT`].
    async fn lock_file(&self) -> Result<FileLock> {
        let path = self.lock_path();
        if let Some(dir) = path.parent() {
            crate::store::create_private_dir(dir)
                .with_context(|| format!("could not create {}", dir.display()))?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(false).write(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let file = options
            .open(&path)
            .with_context(|| format!("could not open {}", path.display()))?;
        let started = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(FileLock(file)),
                Err(std::fs::TryLockError::WouldBlock) if started.elapsed() < LOCK_WAIT => {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    bail!(
                        "{} is locked by another hkc-mcp process; try again",
                        path.display()
                    )
                }
                Err(std::fs::TryLockError::Error(e)) => {
                    return Err(e).with_context(|| format!("could not lock {}", path.display()));
                }
            }
        }
    }

    /// The file's text (empty if it doesn't exist).
    fn read_text(&self) -> Result<String> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => Ok(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e).with_context(|| format!("could not read {}", self.path.display())),
        }
    }

    fn load_unlocked(&self) -> Result<State> {
        let text = self.read_text()?;
        self.parse(&text)
    }

    fn parse(&self, text: &str) -> Result<State> {
        if text.is_empty() {
            return Ok(State::default());
        }
        // A damaged file is an error, never silently replaced: that would
        // drop the password hash.
        let state: State = serde_json::from_str(text)
            .with_context(|| format!("{} is not valid OAuth state", self.path.display()))?;
        if state.version > VERSION {
            bail!(
                "{} was written by a newer hkc-mcp (format {}); upgrade",
                self.path.display(),
                state.version
            );
        }
        Ok(state)
    }

    /// Read the current state.
    pub async fn read<T>(&self, f: impl FnOnce(&State) -> T) -> Result<T> {
        let _guard = self.lock.lock().await;
        Ok(f(&self.load_unlocked()?))
    }

    /// Change the state and save it, under the in-process and the
    /// cross-process lock. Expired tokens are pruned on the way. The file is
    /// rewritten only if its contents change.
    pub async fn update<T>(&self, f: impl FnOnce(&mut State) -> T) -> Result<T> {
        let _guard = self.lock.lock().await;
        let _file_lock = self.lock_file().await?;
        let before = self.read_text()?;
        let mut state = self.parse(&before)?;
        let out = f(&mut state);
        state.prune(Timestamp::now());
        state.version = VERSION;
        let bytes = serde_json::to_vec_pretty(&state)?;
        if bytes != before.as_bytes() {
            write_private(&self.path, &bytes)
                .with_context(|| format!("could not save {}", self.path.display()))?;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tokens_are_stored_hashed_and_families_revoke_together() {
        let dir = tempfile::tempdir().unwrap();
        let file = StateFile::new(dir.path().join("oauth.json"));
        let hour = SignedDuration::from_hours(1);
        let (a, b) = file
            .update(|s| {
                (
                    s.issue("f1", "c", "https://x/mcp", hour, hour),
                    s.issue("f2", "c", "https://x/mcp", hour, hour),
                )
            })
            .await
            .unwrap();
        let text = std::fs::read_to_string(dir.path().join("oauth.json")).unwrap();
        assert!(!text.contains(&a.access_token) && !text.contains(&a.refresh_token));
        let now = Timestamp::now();
        assert!(
            file.read(|s| s.access(&a.access_token, now).is_some())
                .await
                .unwrap()
        );
        // A refresh token is not an access token.
        assert!(
            file.read(|s| s.access(&a.refresh_token, now).is_none())
                .await
                .unwrap()
        );
        assert_eq!(file.update(|s| s.revoke_family("f1")).await.unwrap(), 2);
        assert!(
            file.read(|s| s.access(&a.access_token, now).is_none())
                .await
                .unwrap()
        );
        assert!(
            file.read(|s| s.access(&b.access_token, now).is_some())
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn an_update_that_changes_nothing_does_not_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth.json");
        let file = StateFile::new(path.clone());
        file.update(|s| s.password_hash = Some("h".into()))
            .await
            .unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        for _ in 0..20 {
            file.update(|s| s.tokens.contains_key("nope"))
                .await
                .unwrap();
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
    }

    /// Two handles on one file (as the server and a CLI subcommand would
    /// have), each in its own thread and runtime: no update is lost.
    #[test]
    fn concurrent_handles_do_not_lose_updates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth.json");
        let threads: Vec<_> = (0..2)
            .map(|t| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_time()
                        .build()
                        .unwrap();
                    let file = StateFile::new(path);
                    rt.block_on(async {
                        for i in 0..40 {
                            file.update(|s| {
                                s.clients.insert(
                                    format!("c{t}-{i}"),
                                    RegisteredClient {
                                        redirect_uris: vec![],
                                        client_name: None,
                                        registered_at: Timestamp::now(),
                                    },
                                );
                            })
                            .await
                            .unwrap();
                        }
                    });
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let file = StateFile::new(path);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        assert_eq!(rt.block_on(file.read(|s| s.clients.len())).unwrap(), 80);
    }

    #[tokio::test]
    async fn damaged_file_is_an_error_not_a_reset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth.json");
        std::fs::write(&path, "{ not json").unwrap();
        let file = StateFile::new(path.clone());
        assert!(file.update(|_| ()).await.is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "{ not json");
    }
}
