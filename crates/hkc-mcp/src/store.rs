//! Where `hkc-mcp` keeps its files, and how it writes them.
//!
//! The layout follows `hkc` (same `.env` in the config directory), but the
//! MCP server keeps its own files so the two never fight over a session:
//!
//! - `mcp-session.json`: the saved Bluelink login session (0600).
//! - `mcp-oauth.json`: the sign-in password hash and issued tokens (0600).
//! - `mcp-audit.jsonl`: one line per remote command (0600, append-only).
//!
//! macOS: `~/Library/Application Support/hkconnect/`
//! Linux: `~/.config/hkconnect/` (`.env`) and `~/.local/share/hkconnect/`
//! (everything else). `HKC_MCP_DATA_DIR` moves the data files (useful in a
//! container).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use directories::ProjectDirs;
use hkconnect::{Brand, Session};
use serde::{Deserialize, Serialize};

/// The server's directories.
#[derive(Debug, Clone)]
pub struct Store {
    config_dir: PathBuf,
    data_dir: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct SavedSession {
    brand: String,
    username: String,
    session: Session,
}

impl Store {
    /// The platform directories, with the data directory optionally moved.
    pub fn open(data_dir: Option<PathBuf>) -> Result<Self> {
        let dirs =
            ProjectDirs::from("", "", "hkconnect").context("could not find a home directory")?;
        Ok(Self {
            config_dir: dirs.config_dir().to_owned(),
            data_dir: data_dir.unwrap_or_else(|| dirs.data_dir().to_owned()),
        })
    }

    /// A store that keeps everything in `dir` (for tests).
    #[cfg(test)]
    pub fn at(dir: &Path) -> Self {
        Self {
            config_dir: dir.to_owned(),
            data_dir: dir.to_owned(),
        }
    }

    fn session_file(&self) -> PathBuf {
        self.data_dir.join("mcp-session.json")
    }

    /// The OAuth state file.
    pub fn oauth_file(&self) -> PathBuf {
        self.data_dir.join("mcp-oauth.json")
    }

    /// The audit log.
    pub fn audit_file(&self) -> PathBuf {
        self.data_dir.join("mcp-audit.jsonl")
    }

    /// Read `.env` from the config directory, if there is one. A `.env` in
    /// the current directory is deliberately ignored: running `hkc-mcp` inside
    /// someone else's project must not pick up theirs.
    ///
    /// The file is parsed by [`parse_env`]: values are taken literally,
    /// with no `$VAR` substitution. Problems are reported as warnings on
    /// stderr; values are never printed (the file holds the password).
    pub fn load_env(&self) -> EnvFile {
        let path = self.config_dir.join(".env");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return EnvFile::default(),
            Err(e) => {
                eprintln!("warning: could not read {}: {}", path.display(), e.kind());
                return EnvFile::default();
            }
        };
        if let Some(mode) = loose_permissions(&path) {
            eprintln!(
                "warning: {} is readable by other users (mode {mode:o}) and holds your \
                 password; run `chmod 600` on it",
                path.display()
            );
        }
        let (file, problems) = parse_env(&text);
        for problem in problems {
            eprintln!("warning: {}: {problem}", path.display());
        }
        file
    }

    /// The saved session for this brand and account, if there is one this
    /// version can use.
    pub fn load_session(&self, brand: Brand, username: &str) -> Option<Session> {
        let text = std::fs::read_to_string(self.session_file()).ok()?;
        let saved: SavedSession = serde_json::from_str(&text).ok()?;
        (saved.brand == brand.key()
            && same_username(&saved.username, username)
            && saved.session.is_supported())
        .then_some(saved.session)
    }

    /// Save the session (owner-only, atomically).
    pub fn save_session(&self, brand: Brand, username: &str, session: &Session) -> Result<()> {
        let saved = SavedSession {
            brand: brand.key().into(),
            username: username.into(),
            session: session.clone(),
        };
        let file = self.session_file();
        write_private(&file, serde_json::to_string_pretty(&saved)?.as_bytes())
            .with_context(|| format!("could not save session to {}", file.display()))
    }
}

/// Emails are case-insensitive in practice.
fn same_username(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

/// Settings read from the `.env` file. Real environment variables win over
/// them: see [`EnvFile::var`].
#[derive(Debug, Default)]
pub struct EnvFile(HashMap<String, String>);

impl EnvFile {
    /// `name` from the real environment, or else from the file. Empty values
    /// count as unset.
    pub fn var(&self, name: &str) -> Option<String> {
        self.var_with(name, |n| std::env::var(n).ok())
    }

    fn var_with(&self, name: &str, real: impl Fn(&str) -> Option<String>) -> Option<String> {
        real(name)
            .filter(|v| !v.is_empty())
            .or_else(|| self.0.get(name).filter(|v| !v.is_empty()).cloned())
    }
}

/// Parse a `.env` file strictly and literally.
///
/// - One `KEY=VALUE` per line; `KEY` is letters, digits and `_`, not
///   starting with a digit. An `export ` prefix is allowed.
/// - Blank lines and lines starting with `#` are ignored.
/// - Whitespace around the key and the value is trimmed.
/// - A value may be wrapped in single or double quotes, which are removed;
///   inside them every character is kept as written, including spaces, `#`,
///   `$` and `\`. A comment may follow the closing quote.
/// - Nothing is substituted or unescaped: `$HOME` stays `$HOME` and `\n`
///   stays a backslash and an `n`. A quoted value cannot contain its own
///   quote character; use the other kind.
/// - An unquoted value runs to the end of the line. To avoid reading a
///   trailing comment into a password, an unquoted value containing ` #` is
///   rejected: quote it, or put the comment on its own line.
///
/// Lines that break these rules are skipped, with a problem description
/// (line number and key, never the value) in the second list.
pub fn parse_env(text: &str) -> (EnvFile, Vec<String>) {
    let mut vars = HashMap::new();
    let mut problems = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let n = index + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").map_or(line, str::trim_start);
        let Some((key, value)) = line.split_once('=') else {
            problems.push(format!("line {n}: expected KEY=VALUE; line ignored"));
            continue;
        };
        let key = key.trim();
        if !is_valid_key(key) {
            problems.push(format!("line {n}: invalid variable name; line ignored"));
            continue;
        }
        match parse_env_value(value.trim()) {
            Ok(value) => {
                vars.insert(key.to_owned(), value);
            }
            Err(reason) => problems.push(format!("line {n} ({key}): {reason}; line ignored")),
        }
    }
    (EnvFile(vars), problems)
}

fn is_valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn parse_env_value(value: &str) -> Result<String, &'static str> {
    for quote in ['\'', '"'] {
        if let Some(rest) = value.strip_prefix(quote) {
            let Some(end) = rest.find(quote) else {
                return Err("missing closing quote");
            };
            let after = rest[end + 1..].trim_start();
            if !(after.is_empty() || after.starts_with('#')) {
                return Err("unexpected text after the closing quote");
            }
            return Ok(rest[..end].to_owned());
        }
    }
    if value.contains(" #") || value.contains("\t#") {
        return Err(
            "unquoted value contains ' #'; quote the value or put the comment on its own line",
        );
    }
    Ok(value.to_owned())
}

/// The file's permission bits if group or others can read or write it.
#[cfg(unix)]
fn loose_permissions(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(path).ok()?.permissions().mode() & 0o777;
    (mode & 0o077 != 0).then_some(mode)
}

#[cfg(not(unix))]
fn loose_permissions(_path: &Path) -> Option<u32> {
    None
}

/// Create `dir` (and parents), owner-only on unix. Existing directories are
/// left as they are.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

/// Write a file only the current user can read.
///
/// The bytes go to a freshly created, uniquely named temporary file in the
/// same directory (never an existing file or symlink), which is set to 0600,
/// synced, then atomically renamed over `path`. A crash never leaves a
/// half-written file.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    create_private_dir(dir)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".hkc-mcp-")
        .suffix(".tmp")
        .tempfile_in(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        tmp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        serde_json::from_value(serde_json::json!({
            "access_token": "a",
            "refresh_token": "r",
            "expires_at": "2030-01-01T00:00:00Z",
            "device_id": "d",
        }))
        .unwrap()
    }

    #[test]
    fn session_round_trips_under_its_own_name() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path());
        store
            .save_session(Brand::Hyundai, "Me@Example.com", &session())
            .unwrap();
        assert!(dir.path().join("mcp-session.json").exists());
        assert!(!dir.path().join("session.json").exists());
        assert_eq!(
            store.load_session(Brand::Hyundai, "me@example.com"),
            Some(session())
        );
        assert_eq!(store.load_session(Brand::Kia, "me@example.com"), None);
    }

    #[cfg(unix)]
    #[test]
    fn files_and_new_directories_are_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let file = data.join("x.json");
        write_private(&file, b"{}").unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(&data), 0o700);
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&file, b"{}").unwrap();
        assert_eq!(mode(&file), 0o600);
    }

    fn parsed(text: &str) -> (HashMap<String, String>, Vec<String>) {
        let (file, problems) = parse_env(text);
        (file.0, problems)
    }

    #[test]
    fn env_values_are_literal() {
        let (vars, problems) = parsed(
            "HKC_PASSWORD=pa$word1\n\
             A=\"pa$word1 #not a comment\"\n\
             B='${HOME}\\n'\n\
             C=\"it's\"\n\
             D='say \"hi\"'   # trailing comment\n\
             E=  spaced out  \n\
             export F=exported\r\n\
             G=\n\
             H=a=b\n",
        );
        assert_eq!(problems.len(), 0, "{problems:?}");
        assert_eq!(vars["HKC_PASSWORD"], "pa$word1");
        assert_eq!(vars["A"], "pa$word1 #not a comment");
        assert_eq!(vars["B"], "${HOME}\\n");
        assert_eq!(vars["C"], "it's");
        assert_eq!(vars["D"], "say \"hi\"");
        assert_eq!(vars["E"], "spaced out");
        assert_eq!(vars["F"], "exported");
        assert_eq!(vars["G"], "");
        assert_eq!(vars["H"], "a=b");
    }

    #[test]
    fn env_comments_and_blank_lines_are_skipped() {
        let (vars, problems) = parsed("# HKC_PIN=1111\n\n   \n  # indented\nHKC_PIN=2222\n");
        assert_eq!(problems.len(), 0, "{problems:?}");
        assert_eq!(vars.len(), 1);
        assert_eq!(vars["HKC_PIN"], "2222");
    }

    #[test]
    fn malformed_env_lines_are_skipped_without_echoing_values() {
        let (vars, problems) = parsed(
            "hunter2-no-equals\n\
             1BAD=hunter2\n\
             BAD KEY=hunter2\n\
             OPEN=\"hunter2\n\
             TAIL='hunter2' extra\n\
             PIN=1234 # my pin\n\
             GOOD=ok\n",
        );
        assert_eq!(vars.len(), 1, "{vars:?}");
        assert_eq!(vars["GOOD"], "ok");
        assert_eq!(problems.len(), 6, "{problems:?}");
        for (problem, line) in problems.iter().zip(1..) {
            assert!(problem.contains(&format!("line {line}")), "{problem}");
            assert!(
                !problem.contains("hunter2") && !problem.contains("1234"),
                "{problem}"
            );
        }
        assert!(problems[3].contains("OPEN"), "{}", problems[3]);
    }

    #[test]
    fn real_environment_wins_over_the_file() {
        let (file, _) = parse_env("HKC_USERNAME=file@example.com\nHKC_PIN=1111\n");
        let real = |name: &str| match name {
            "HKC_USERNAME" => Some("real@example.com".to_owned()),
            "HKC_PIN" => Some(String::new()),
            _ => None,
        };
        assert_eq!(
            file.var_with("HKC_USERNAME", real).as_deref(),
            Some("real@example.com")
        );
        // An empty real variable counts as unset.
        assert_eq!(file.var_with("HKC_PIN", real).as_deref(), Some("1111"));
        assert_eq!(file.var_with("HKC_BRAND", real), None);
    }

    #[test]
    fn load_env_reads_the_config_directory() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path());
        assert_eq!(store.load_env().0.len(), 0);
        std::fs::write(dir.path().join(".env"), "HKC_PASSWORD='pa$word1'\n").unwrap();
        assert_eq!(store.load_env().0["HKC_PASSWORD"], "pa$word1");
    }
}
