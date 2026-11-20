//! TLS is mandatory. Generated identities are persistent, never silently replaced.
use crate::{
    config::Config,
    error::{AppError, Result},
    fs::{Disk, blocking},
};
use axum_server::tls_rustls::RustlsConfig;
use serde::{Deserialize, Serialize};
use std::{io::Read, path::Path};

const IDENTITY: &str = "gc/tls-identity.json";
pub const PUBLIC_CERT: &str = "gc/tls-cert.pem";
const DAY: u64 = 86400;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    host: String,
    expires_at: u64,
    certificate: String,
    private_key: String,
}

fn generated(disk: &Disk, host: &str) -> Result<Identity> {
    let _lock = disk.lock("tls-identity", true)?;
    let now = crate::now();
    let identity = match disk.open_read(Path::new(IDENTITY)) {
        Ok(file) => {
            let mut bytes = Vec::new();
            file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
            if bytes.len() > 1024 * 1024 {
                return Err(AppError::unavailable("generated TLS identity is too large"));
            }
            serde_json::from_slice::<Identity>(&bytes)?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let key = rcgen::KeyPair::generate().map_err(AppError::internal)?;
            let mut params =
                rcgen::CertificateParams::new(vec![host.to_owned()]).map_err(AppError::internal)?;
            params
                .distinguished_name
                .push(rcgen::DnType::CommonName, host);
            params.not_before =
                time::OffsetDateTime::from_unix_timestamp(now.saturating_sub(300) as i64)
                    .map_err(AppError::internal)?;
            params.not_after = time::OffsetDateTime::from_unix_timestamp((now + 365 * DAY) as i64)
                .map_err(AppError::internal)?;
            params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
            let certificate = params.self_signed(&key).map_err(AppError::internal)?;
            let identity = Identity {
                host: host.to_owned(),
                expires_at: now + 365 * DAY,
                certificate: certificate.pem(),
                private_key: key.serialize_pem(),
            };
            disk.atomic(Path::new(IDENTITY), &serde_json::to_vec(&identity)?)?;
            tracing::warn!(
                host,
                certificate = PUBLIC_CERT,
                "generated a self-signed TLS certificate; clients must explicitly trust it"
            );
            identity
        }
        Err(error) => return Err(error.into()),
    };
    if identity.host != host {
        return Err(AppError::bad(
            "TLS_INVALID",
            "generated certificate hostname differs from publicUrl; explicit certificate migration required",
        ));
    }
    if now >= identity.expires_at {
        return Err(AppError::bad(
            "TLS_INVALID",
            "generated TLS certificate expired; supply a renewed certificate/key or explicitly rotate the generated identity and update client trust",
        ));
    }
    tracing::info!(
        host,
        expires_at = identity.expires_at,
        "using persisted self-signed TLS identity"
    );
    if identity.expires_at - now < 30 * DAY {
        tracing::warn!(
            expires_at = identity.expires_at,
            "generated TLS certificate expires within 30 days; plan renewal and client trust updates"
        );
    }
    // Export only the public certificate for Docker/client trust installation.
    // The key remains inside the atomically stored, mode-0600 identity bundle.
    let public = match disk.open_read(Path::new(PUBLIC_CERT)) {
        Ok(mut f) => {
            let mut bytes = Vec::new();
            f.read_to_end(&mut bytes)?;
            Some(bytes)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if public.as_deref() != Some(identity.certificate.as_bytes()) {
        disk.atomic(Path::new(PUBLIC_CERT), identity.certificate.as_bytes())?;
    }
    Ok(identity)
}

pub async fn load(config: &Config, disk: Disk) -> Result<RustlsConfig> {
    config.validate()?;
    // Exactly one TLS crypto backend; no OpenSSL/shared-library dependency.
    let _ = rustls::crypto::ring::default_provider().install_default();
    match (&config.tls_cert, &config.tls_key) {
        (Some(cert), Some(key)) => RustlsConfig::from_pem_file(cert, key).await.map_err(|_| {
            AppError::bad(
                "TLS_INVALID",
                "unable to load TLS certificate/key; files must contain a valid matching PEM pair",
            )
        }),
        (None, None) => {
            let host = config
                .public_url
                .host()
                .ok_or_else(|| AppError::bad("TLS_INVALID", "publicUrl requires a hostname"))?
                .to_string();
            let identity = blocking(move || generated(&disk, &host)).await?;
            RustlsConfig::from_pem(
                identity.certificate.into_bytes(),
                identity.private_key.into_bytes(),
            )
            .await
            .map_err(|_| {
                AppError::bad(
                    "TLS_INVALID",
                    "stored TLS identity is invalid; refusing to replace it silently",
                )
            })
        }
        _ => Err(AppError::bad(
            "TLS_INVALID",
            "tlsCert and tlsKey must be supplied together",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn config(root: &Path) -> Config {
        let mut config = crate::auth::fixtures::config();
        config.data_root = root.into();
        config.public_url = "https://localhost".parse().unwrap();
        config
    }
    #[tokio::test]
    async fn generated_identity_is_shared_and_private() {
        let root = tempfile::tempdir().unwrap();
        let disk = Disk::open(root.path()).unwrap();
        let config = config(root.path());
        let (a, b) = tokio::join!(load(&config, disk.clone()), load(&config, disk.clone()));
        a.unwrap();
        b.unwrap();
        let before = disk.read(Path::new(IDENTITY)).unwrap();
        load(&config, disk.clone()).await.unwrap();
        assert_eq!(disk.read(Path::new(IDENTITY)).unwrap(), before);
        assert_eq!(
            std::fs::metadata(root.path().join(IDENTITY))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let public = disk.read(Path::new(PUBLIC_CERT)).unwrap();
        assert!(
            String::from_utf8(public)
                .unwrap()
                .contains("BEGIN CERTIFICATE")
        );
    }
    #[tokio::test]
    async fn provided_pair_and_invalid_keys_never_fall_back() {
        let root = tempfile::tempdir().unwrap();
        let disk = Disk::open(root.path()).unwrap();
        let mut config = config(root.path());
        let identity = generated(&disk, "localhost").unwrap();
        let cert = root.path().join("cert.pem");
        let key = root.path().join("key.pem");
        std::fs::write(&cert, identity.certificate).unwrap();
        std::fs::write(&key, identity.private_key).unwrap();
        config.tls_cert = Some(cert);
        config.tls_key = Some(key.clone());
        load(&config, disk.clone()).await.unwrap();
        std::fs::write(key, "invalid key").unwrap();
        assert!(load(&config, disk.clone()).await.is_err());
        config.tls_key = None;
        assert!(config.validate().is_err());
        assert!(load(&config, disk).await.is_err());
    }
    #[tokio::test]
    async fn corrupted_expired_or_wrong_host_identity_is_not_regenerated() {
        let root = tempfile::tempdir().unwrap();
        let disk = Disk::open(root.path()).unwrap();
        let config = config(root.path());
        let mut identity = generated(&disk, "localhost").unwrap();
        identity.expires_at = 0;
        disk.atomic(Path::new(IDENTITY), &serde_json::to_vec(&identity).unwrap())
            .unwrap();
        assert!(load(&config, disk.clone()).await.is_err());
        identity.expires_at = crate::now() + DAY;
        identity.host = "other.example".into();
        disk.atomic(Path::new(IDENTITY), &serde_json::to_vec(&identity).unwrap())
            .unwrap();
        assert!(load(&config, disk.clone()).await.is_err());
        disk.atomic(Path::new(IDENTITY), b"corrupt").unwrap();
        assert!(load(&config, disk.clone()).await.is_err());
        assert_eq!(disk.read(Path::new(IDENTITY)).unwrap(), b"corrupt");
    }
    // Requires local sockets. Kept explicit for restricted sandbox test runs.
    #[tokio::test]
    #[ignore]
    async fn https_works_and_plaintext_is_rejected() {
        use axum::{Router, routing::get};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let root = tempfile::tempdir().unwrap();
        let disk = Disk::open(root.path()).unwrap();
        let config = config(root.path());
        let tls = load(&config, disk.clone()).await.unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let handle = axum_server::Handle::new();
        let control = handle.clone();
        let server = tokio::spawn(async move {
            axum_server::from_tcp_rustls(listener, tls)
                .unwrap()
                .handle(handle)
                .serve(
                    Router::new()
                        .route("/", get(|| async { "encrypted" }))
                        .into_make_service(),
                )
                .await
                .unwrap();
        });
        let cert =
            reqwest::Certificate::from_pem(&disk.read(Path::new(PUBLIC_CERT)).unwrap()).unwrap();
        let client = reqwest::Client::builder()
            .add_root_certificate(cert)
            .resolve("localhost", address)
            .no_proxy()
            .build()
            .unwrap();
        let response = client
            .get(format!("https://localhost:{}/", address.port()))
            .send()
            .await
            .unwrap();
        assert_eq!(response.text().await.unwrap(), "encrypted");
        let mut plain = tokio::net::TcpStream::connect(address).await.unwrap();
        plain
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut bytes = Vec::new();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            plain.read_to_end(&mut bytes),
        )
        .await;
        assert!(
            !bytes.windows(4).any(|b| b == b"HTTP"),
            "server answered plaintext HTTP"
        );
        control.graceful_shutdown(Some(std::time::Duration::from_secs(1)));
        server.await.unwrap();
    }
}
