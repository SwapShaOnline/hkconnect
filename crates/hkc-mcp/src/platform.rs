//! Defaults supplied by a hosting platform, so the container image starts
//! with no flags there. Today that is only Railway.
//!
//! Railway sets `PORT` (its proxy forwards only to that port),
//! `RAILWAY_PUBLIC_DOMAIN` (the service's public host name, once one is
//! generated) and terminates TLS at an edge proxy that is the only way in
//! from the internet. Outside Railway nothing here changes behaviour.

use std::net::SocketAddr;

use anyhow::{Context as _, Result};

/// Variables whose presence means the process runs on Railway.
const RAILWAY_MARKERS: [&str; 3] = [
    "RAILWAY_ENVIRONMENT_ID",
    "RAILWAY_SERVICE_ID",
    "RAILWAY_PUBLIC_DOMAIN",
];

/// What Railway implies for `serve`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Railway {
    /// `0.0.0.0:$PORT`, when Railway gave a port.
    pub listen: Option<SocketAddr>,
    /// `https://$RAILWAY_PUBLIC_DOMAIN`, when there is a public domain.
    pub public_url: Option<String>,
}

/// Railway's defaults, or `None` when not on Railway. `var` looks a
/// variable up (empty counts as unset).
pub fn railway(var: impl Fn(&str) -> Option<String>) -> Result<Option<Railway>> {
    if !RAILWAY_MARKERS.iter().any(|name| var(name).is_some()) {
        return Ok(None);
    }
    let listen = var("PORT")
        .map(|p| {
            p.trim()
                .parse::<u16>()
                .ok()
                .filter(|p| *p != 0)
                .map(|p| SocketAddr::from(([0, 0, 0, 0], p)))
                .context("invalid PORT: use a port number")
        })
        .transpose()?;
    let public_url = var("RAILWAY_PUBLIC_DOMAIN").map(|d| {
        let d = d.trim().trim_end_matches('/');
        let d = d.strip_prefix("https://").unwrap_or(d);
        format!("https://{d}")
    });
    Ok(Some(Railway { listen, public_url }))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| map.get(name).filter(|v| !v.is_empty()).cloned()
    }

    #[test]
    fn nothing_outside_railway() {
        // A stray PORT in a shell must not move a self-hosted server off
        // loopback.
        assert_eq!(railway(lookup(&[("PORT", "3000")])).unwrap(), None);
        assert_eq!(railway(lookup(&[])).unwrap(), None);
    }

    #[test]
    fn railway_port_and_domain() {
        let r = railway(lookup(&[
            ("RAILWAY_ENVIRONMENT_ID", "e"),
            ("RAILWAY_PUBLIC_DOMAIN", "hkc-mcp-production.up.railway.app"),
            ("PORT", "8080"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(r.listen, Some("0.0.0.0:8080".parse().unwrap()));
        assert_eq!(
            r.public_url.as_deref(),
            Some("https://hkc-mcp-production.up.railway.app")
        );
        // The derived URL is one `serve` accepts.
        crate::oauth::OAuthConfig::new(r.public_url.as_deref().unwrap()).unwrap();
    }

    #[test]
    fn railway_without_domain_or_port() {
        let r = railway(lookup(&[("RAILWAY_SERVICE_ID", "s")]))
            .unwrap()
            .unwrap();
        assert_eq!(r, Railway::default());
    }

    #[test]
    fn bad_port_is_an_error() {
        let err = railway(lookup(&[("RAILWAY_SERVICE_ID", "s"), ("PORT", "http")])).unwrap_err();
        assert!(err.to_string().contains("PORT"));
        assert!(railway(lookup(&[("RAILWAY_SERVICE_ID", "s"), ("PORT", "0")])).is_err());
    }
}
