use crate::error::{AppError, Result};
use serde::Deserialize;
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};
use url::Url;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    #[serde(default = "listen")]
    pub listen: SocketAddr,
    pub data_root: PathBuf,
    pub public_url: Url,
    pub gitlab_issuer: Url,
    pub internal_ca: Option<PathBuf>,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
}
fn listen() -> SocketAddr {
    "0.0.0.0:443".parse().unwrap()
}
impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let config: Self =
            serde_yaml::from_slice(&std::fs::read(path)?).map_err(AppError::internal)?;
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&self) -> Result<()> {
        if self.tls_cert.is_some() != self.tls_key.is_some() {
            return Err(AppError::bad(
                "CONFIG_INVALID",
                "tlsCert and tlsKey must be provided together, or omitted to generate a self-signed certificate",
            ));
        }
        for (name, url) in [
            ("publicUrl", &self.public_url),
            ("gitlabIssuer", &self.gitlab_issuer),
        ] {
            if url.scheme() != "https"
                || url.host_str().is_none()
                || url.path() != "/"
                || url.query().is_some()
                || url.fragment().is_some()
                || !url.username().is_empty()
                || url.password().is_some()
            {
                return Err(AppError::bad(
                    "CONFIG_INVALID",
                    format!(
                        "{name} must be an HTTPS origin without credentials, path, query, or fragment"
                    ),
                ));
            }
        }
        if !self.data_root.is_absolute()
            || self
                .data_root
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(AppError::bad(
                "CONFIG_INVALID",
                "dataRoot must be absolute without parent traversal",
            ));
        }
        Ok(())
    }
    pub fn audience(&self) -> String {
        self.public_url.as_str().trim_end_matches('/').to_owned()
    }
    pub fn issuer(&self) -> String {
        self.gitlab_issuer.as_str().trim_end_matches('/').to_owned()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_configuration() {
        assert!(
            serde_yaml::from_str::<Config>(
                "dataRoot: /tmp/data\npublicUrl: https://registry.example\ngitlabIssuer: https://gitlab.example\nunknown: true"
            )
            .is_err()
        );
        assert!(
            serde_yaml::from_str::<Config>(
                "dataRoot: /tmp/data\npublicUrl: https://registry.example"
            )
            .is_err()
        );
        let mut c: Config =
            serde_yaml::from_str("dataRoot: /tmp/data\npublicUrl: https://registry.example\ngitlabIssuer: https://gitlab.example")
                .unwrap();
        c.validate().unwrap();
        assert_eq!(c.listen.to_string(), "0.0.0.0:443");
        c.tls_cert = Some("cert.pem".into());
        assert!(c.validate().is_err());
        c.tls_cert = None;
        c.public_url = Url::parse("https://example/path").unwrap();
        assert!(c.validate().is_err());
    }
}
