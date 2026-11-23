//! Per-brand connection settings for the India servers.
//!
//! These identifiers ship inside the official Android apps; they identify the
//! app, not the user.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use url::Url;

/// Car brand. Hyundai and Kia India run the same protocol on different hosts.
///
/// Parses case-insensitively from its [`key`](Brand::key), `"hyundai"` or
/// `"kia"`, and displays as that key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Brand {
    /// Hyundai Bluelink India.
    Hyundai,
    /// Kia Connect India.
    Kia,
}

impl Brand {
    /// A stable lowercase name for the brand: `"hyundai"` or `"kia"`.
    /// Suitable for config files and for keying saved sessions.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Hyundai => "hyundai",
            Self::Kia => "kia",
        }
    }
}

impl std::fmt::Display for Brand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.key())
    }
}

impl std::str::FromStr for Brand {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "hyundai" => Ok(Self::Hyundai),
            "kia" => Ok(Self::Kia),
            other => Err(format!("unknown brand {other:?} (expected hyundai or kia)")),
        }
    }
}

/// Everything needed to talk to one brand's India API.
///
/// Build one with [`Config::india`]. Its `Debug` output redacts the app's
/// basic-auth secret and signing key.
#[derive(Clone)]
pub struct Config {
    pub(crate) brand: Brand,
    pub(crate) base_url: Url,
    pub(crate) service_id: &'static str,
    pub(crate) app_id: &'static str,
    pub(crate) basic_auth: &'static str,
    pub(crate) push_type: &'static str,
    pub(crate) cfb: Vec<u8>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("brand", &self.brand)
            .field("base_url", &self.base_url.as_str())
            .field("service_id", &self.service_id)
            .field("app_id", &self.app_id)
            .field("basic_auth", &"[redacted]")
            .field("push_type", &self.push_type)
            .field("cfb", &"[redacted]")
            .finish()
    }
}

impl Config {
    /// Production settings for Bluelink India (Hyundai) or Kia Connect India.
    #[must_use]
    #[expect(
        clippy::missing_panics_doc,
        reason = "only parses built-in constants, covered by tests"
    )]
    pub fn india(brand: Brand) -> Self {
        let (host, service_id, app_id, cfb, basic_auth, push_type) = match brand {
            Brand::Hyundai => (
                "prd.in-ccapi.hyundai.connected-car.io",
                "e5b3f6d0-7f83-43c9-aff3-a254db7af368",
                "5a27df80-4ca1-4154-8c09-6f4029d91cf7",
                "RFtoRq/vDXJmRndoZaZQyfOot7OrIqGVFj96iY2WL3yyH5Z/pUvlUhqmCxD2t+D65SQ=",
                "Basic ZTViM2Y2ZDAtN2Y4My00M2M5LWFmZjMtYTI1NGRiN2FmMzY4OjVKRk9DcjZDMjRPZk96bERxWnA3RXdxcmtMMFd3MDRVYXhjRGlFNlVkM3FJNVNFNA==",
                "GCM",
            ),
            Brand::Kia => (
                "prd.in-ccapi.kia.connected-car.io",
                "d0fe4855-7527-4be0-ab6e-a481216c705d",
                "00000000-69cd-4660-b75d-277ae15379dd",
                "pdfn/jCrrEcxH6Jnak/1O/DaD+HjVh0P6z/BHWNoUKQtT0aLcYwer8BxQOoiHXSyMtBV",
                "Basic ZDBmZTQ4NTUtNzUyNy00YmUwLWFiNmUtYTQ4MTIxNmM3MDVkOlNIb1R0WHB5ZmJZbVAzWGpOQTZCcnRsRGdseXBQV2o5MjBQdEtCSlBmbGVIRVlwVQ==",
                "APNS",
            ),
        };
        Self {
            brand,
            base_url: Url::parse(&format!("https://{host}:8080"))
                .expect("static base URL is valid"),
            service_id,
            app_id,
            basic_auth,
            push_type,
            cfb: STANDARD
                .decode(cfb)
                .expect("static CFB key is valid base64"),
        }
    }

    /// Point the client at a different server (used by tests and proxies).
    #[must_use]
    pub fn with_base_url(mut self, base_url: Url) -> Self {
        self.base_url = base_url;
        self
    }

    /// The brand this configuration is for.
    #[must_use]
    pub fn brand(&self) -> Brand {
        self.brand
    }

    pub(crate) fn url(&self, path: &str) -> Url {
        self.base_url.join(path).expect("API paths are valid")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brand_key_display_and_parse_agree() {
        for brand in [Brand::Hyundai, Brand::Kia] {
            assert_eq!(brand.to_string(), brand.key());
            assert_eq!(brand.key().parse::<Brand>(), Ok(brand));
            assert_eq!(brand.key().to_uppercase().parse::<Brand>(), Ok(brand));
        }
        assert!("tata".parse::<Brand>().is_err());
    }
}
