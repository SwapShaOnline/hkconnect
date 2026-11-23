//! Which tools the server offers.
//!
//! Read tools and the low-risk commands are on by default. Unlocking, remote
//! start, hazard lights and valet mode are off unless named in
//! `HKC_MCP_ENABLE`. `HKC_MCP_DISABLE` turns default tools off. A disabled
//! tool is not advertised at all, so a client cannot even see it.
//!
//! To change the defaults, edit [`DEFAULT_ON`] and [`OPT_IN`].

use std::collections::BTreeSet;

use anyhow::{Result, bail};

/// Tools that are on unless listed in `HKC_MCP_DISABLE`.
pub const DEFAULT_ON: &[&str] = &[
    "list_vehicles",
    "get_status",
    "get_location",
    "get_vehicle_info",
    "get_trips",
    "check_command",
    "lock",
    "stop_climate",
    "send_destination",
];

/// Tools that are off unless listed in `HKC_MCP_ENABLE`.
pub const OPT_IN: &[&str] = &["unlock", "start_climate", "hazard_lights", "set_valet_mode"];

/// The set of tools to advertise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    enabled: BTreeSet<&'static str>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_ON.iter().copied().collect(),
        }
    }
}

impl Policy {
    /// Build from the comma-separated `HKC_MCP_ENABLE` and `HKC_MCP_DISABLE`
    /// values. Names are case-insensitive; `all` in `enable` turns on every
    /// tool. Unknown names are an error, so a typo can't silently leave a
    /// tool on or off.
    pub fn from_lists(enable: Option<&str>, disable: Option<&str>) -> Result<Self> {
        let mut policy = Self::default();
        for name in names(enable) {
            if name == "all" {
                policy.enabled.extend(all_tools());
                continue;
            }
            policy.enabled.insert(known(&name)?);
        }
        for name in names(disable) {
            policy.enabled.remove(known(&name)?);
        }
        Ok(policy)
    }

    /// Whether `tool` is offered.
    pub fn allows(&self, tool: &str) -> bool {
        self.enabled.contains(tool)
    }

    /// The tools offered, sorted.
    pub fn enabled(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.enabled.iter().copied()
    }
}

fn all_tools() -> impl Iterator<Item = &'static str> {
    DEFAULT_ON.iter().chain(OPT_IN).copied()
}

fn names(list: Option<&str>) -> impl Iterator<Item = String> + '_ {
    list.unwrap_or("")
        .split(',')
        .map(|n| n.trim().to_ascii_lowercase())
        .filter(|n| !n.is_empty())
}

fn known(name: &str) -> Result<&'static str> {
    match all_tools().find(|t| *t == name) {
        Some(t) => Ok(t),
        None => bail!(
            "unknown tool {name:?} in HKC_MCP_ENABLE / HKC_MCP_DISABLE; known tools: {}",
            all_tools().collect::<Vec<_>>().join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn risky_tools_are_off_by_default() {
        let p = Policy::default();
        for t in OPT_IN {
            assert!(!p.allows(t), "{t}");
        }
        for t in DEFAULT_ON {
            assert!(p.allows(t), "{t}");
        }
    }

    #[test]
    fn enable_and_disable_lists() {
        let p = Policy::from_lists(Some(" Unlock, start_climate ,"), Some("lock")).unwrap();
        assert!(p.allows("unlock") && p.allows("start_climate"));
        assert!(!p.allows("lock") && !p.allows("hazard_lights"));
        let all = Policy::from_lists(Some("all"), None).unwrap();
        assert_eq!(all.enabled().count(), DEFAULT_ON.len() + OPT_IN.len());
    }

    #[test]
    fn typos_are_errors() {
        let e = Policy::from_lists(Some("unlok"), None).unwrap_err();
        assert!(e.to_string().contains("unlok"));
        assert!(Policy::from_lists(None, Some("nope")).is_err());
    }
}
