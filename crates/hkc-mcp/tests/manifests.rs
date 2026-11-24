//! The Claude Code plugin manifest and the MCP Registry `server.json` at the
//! repository root must carry the crate's version, or plugin users miss
//! updates and the registry entry points at an old release. Bump them with
//! the workspace version (see CONTRIBUTING.md, "Releasing").

use std::path::PathBuf;

use serde_json::Value;

fn repo_file(name: &str) -> Option<Value> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(name);
    // Not part of the published crate: skip when built from crates.io.
    let text = std::fs::read_to_string(path).ok()?;
    Some(serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name} is not valid JSON: {e}")))
}

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[test]
fn plugin_manifest_version_matches_crate() {
    let Some(plugin) = repo_file(".claude-plugin/plugin.json") else {
        return;
    };
    assert_eq!(
        plugin["version"], VERSION,
        ".claude-plugin/plugin.json version"
    );
}

#[test]
fn registry_server_json_version_matches_crate() {
    let Some(server) = repo_file("server.json") else {
        return;
    };
    assert_eq!(server["version"], VERSION, "server.json version");
    for package in server["packages"].as_array().expect("server.json packages") {
        assert_eq!(package["version"], VERSION, "server.json package version");
        assert_eq!(package["identifier"], env!("CARGO_PKG_NAME"));
    }
}

#[test]
fn readme_declares_registry_name() {
    let Some(server) = repo_file("server.json") else {
        return;
    };
    let name = server["name"].as_str().expect("server.json name");
    let readme =
        std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("README.md"))
            .expect("README.md");
    assert!(
        readme.contains(&format!("mcp-name: {name}`")),
        "README.md must contain a visible `mcp-name: {name}` line for the MCP Registry"
    );
}
