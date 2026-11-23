//! Where `hkc` keeps its files: a saved session and an optional `.env`.
//!
//! macOS: `~/Library/Application Support/hkconnect/`
//! Linux: `~/.config/hkconnect/` and `~/.local/share/hkconnect/`

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use directories::ProjectDirs;
use hkconnect::{Brand, Session};
use serde::{Deserialize, Serialize};

pub struct Store {
    config_dir: PathBuf,
    session_file: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct SavedSession {
    brand: String,
    username: String,
    session: Session,
}

impl Store {
    pub fn open() -> Result<Self> {
        let dirs =
            ProjectDirs::from("", "", "hkconnect").context("could not find a home directory")?;
        Ok(Self {
            config_dir: dirs.config_dir().to_owned(),
            session_file: dirs.data_dir().join("session.json"),
        })
    }

    /// A store that keeps everything in `dir` (for tests).
    #[cfg(test)]
    pub fn at(dir: &Path) -> Self {
        Self {
            config_dir: dir.to_owned(),
            session_file: dir.join("session.json"),
        }
    }

    /// Read `.env` from the config directory, if there is one. A `.env` in
    /// the current directory is deliberately ignored: running `hkc` inside
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
    /// version can use. A session in a format from a newer release is
    /// ignored, as if there were none (the next save replaces it).
    pub fn load_session(&self, brand: Brand, username: &str) -> Option<Session> {
        let text = std::fs::read_to_string(&self.session_file).ok()?;
        let saved: SavedSession = serde_json::from_str(&text).ok()?;
        (saved.brand == brand.key()
            && same_username(&saved.username, username)
            && saved.session.is_supported())
        .then_some(saved.session)
    }

    pub fn save_session(&self, brand: Brand, username: &str, session: &Session) -> Result<()> {
        let saved = SavedSession {
            brand: brand.key().into(),
            username: username.into(),
            session: session.clone(),
        };
        write_private(
            &self.session_file,
            serde_json::to_string_pretty(&saved)?.as_bytes(),
        )
        .with_context(|| format!("could not save session to {}", self.session_file.display()))
    }

    pub fn forget_session(&self) -> Result<()> {
        match std::fs::remove_file(&self.session_file) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
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

/// Emails are case-insensitive in practice.
fn same_username(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
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
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

/// Write a file only the current user can read (it holds login tokens).
///
/// The bytes go to a freshly created, uniquely named temporary file in the
/// same directory (never an existing file or symlink), which is set to 0600,
/// synced, then atomically renamed over `path`. A crash never leaves a
/// half-written file, and concurrent runs don't clobber each other.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    create_private_dir(dir)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".session-")
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
    fn round_trips_and_matches_brand_and_username() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path());
        store
            .save_session(Brand::Hyundai, "Me@Example.com", &session())
            .unwrap();
        assert_eq!(
            store.load_session(Brand::Hyundai, "me@example.com"),
            Some(session())
        );
        assert_eq!(store.load_session(Brand::Kia, "me@example.com"), None);
        assert_eq!(
            store.load_session(Brand::Hyundai, "other@example.com"),
            None
        );
    }

    #[test]
    fn sessions_from_a_newer_format_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path());
        store
            .save_session(Brand::Hyundai, "me@example.com", &session())
            .unwrap();
        let mut saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&store.session_file).unwrap()).unwrap();
        assert_eq!(saved["brand"], "hyundai");
        assert_eq!(saved["session"]["version"], Session::FORMAT_VERSION);
        saved["session"]["version"] = (Session::FORMAT_VERSION + 1).into();
        std::fs::write(&store.session_file, saved.to_string()).unwrap();
        assert_eq!(store.load_session(Brand::Hyundai, "me@example.com"), None);
    }

    #[test]
    fn load_without_a_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path());
        assert_eq!(store.load_session(Brand::Hyundai, "me@example.com"), None);
    }

    #[test]
    fn forget_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path());
        store.forget_session().unwrap();
        store
            .save_session(Brand::Kia, "me@example.com", &session())
            .unwrap();
        store.forget_session().unwrap();
        store.forget_session().unwrap();
        assert_eq!(store.load_session(Brand::Kia, "me@example.com"), None);
    }

    #[test]
    fn creates_missing_directories() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(&dir.path().join("a").join("b"));
        store
            .save_session(Brand::Hyundai, "me@example.com", &session())
            .unwrap();
        assert!(
            store
                .load_session(Brand::Hyundai, "me@example.com")
                .is_some()
        );
    }

    #[cfg(unix)]
    #[test]
    fn saved_session_is_owner_only_even_over_a_loose_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("session.json");
        std::fs::write(&file, "{}").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        // A stale temp file from the old scheme must not matter either.
        std::fs::write(dir.path().join("session.tmp"), "{}").unwrap();
        std::fs::set_permissions(
            dir.path().join("session.tmp"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();

        Store::at(dir.path())
            .save_session(Brand::Hyundai, "me@example.com", &session())
            .unwrap();
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // No temp files left behind besides the pre-existing one.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.starts_with(".session-"))
            .collect();
        assert_eq!(leftovers.len(), 0, "{leftovers:?}");
    }

    #[cfg(unix)]
    #[test]
    fn new_data_directory_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("hkconnect");
        Store::at(&data)
            .save_session(Brand::Hyundai, "me@example.com", &session())
            .unwrap();
        let mode = std::fs::metadata(&data).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn flags_group_or_world_readable_env() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let env = dir.path().join(".env");
        std::fs::write(&env, "").unwrap();
        std::fs::set_permissions(&env, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(loose_permissions(&env), Some(0o644));
        std::fs::set_permissions(&env, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(loose_permissions(&env), None);
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
