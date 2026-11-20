use crate::{
    config::Config,
    error::{AppError, Result},
    fs::Disk,
    store::{Owner, check_name, valid_name},
};
use axum::http::HeaderMap;
use base64::{Engine, engine::general_purpose::STANDARD};
use jsonwebtoken::{
    Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, decode_header, encode,
    jwk::JwkSet,
};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, sync::Arc};
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct Auth {
    config: Config,
    client: reqwest::Client,
    keys: Arc<Mutex<KeyCache>>,
    secret: Arc<Vec<u8>>,
}
struct KeyCache {
    keys: JwkSet,
    fetched: u64,
    attempted: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Actor {
    pub owner: Owner,
    pub level: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Access {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub actions: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub iss: String,
    pub aud: String,
    pub sub: String,
    pub exp: u64,
    pub nbf: u64,
    pub iat: u64,
    pub jti: String,
    pub access: Vec<Access>,
    pub actor: Option<Actor>,
}
#[derive(Deserialize)]
struct GitlabClaims {
    exp: u64,
    iat: u64,
    sub: String,
    #[serde(deserialize_with = "number")]
    job_project_id: u64,
    job_project_path: String,
    #[serde(deserialize_with = "number")]
    project_id: u64,
    project_path: String,
    #[serde(deserialize_with = "access_level")]
    user_access_level: u64,
    pipeline_source: String,
}
fn number<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<u64, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .ok_or_else(|| serde::de::Error::custom("expected unsigned number or numeric string"))
}
// GitLab emits lowercase role names; retain compatibility with older numeric claims.
fn access_level<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<u64, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    match &value {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(role) => match role.as_str() {
            "minimal_access" => Some(5),
            "guest" => Some(10),
            "planner" => Some(15),
            "reporter" => Some(20),
            "developer" => Some(30),
            "maintainer" => Some(40),
            "owner" => Some(50),
            _ => role.parse::<u64>().ok(),
        },
        _ => None,
    }
    .ok_or_else(|| {
        tracing::info!(
            reason = "invalid_user_access_level",
            "GitLab CI token refused"
        );
        serde::de::Error::custom("expected a GitLab role name or unsigned access level")
    })
}

// Do not format arbitrary JWT/serde errors: their messages can echo supplied claim values.
fn refused(reason: &'static str) -> AppError {
    tracing::info!(reason, "GitLab CI token refused");
    AppError::unauthorized()
}
fn jwt_refused(error: jsonwebtoken::errors::Error) -> AppError {
    use jsonwebtoken::errors::ErrorKind;
    let reason = match error.kind() {
        ErrorKind::Json(error) => {
            tracing::info!(category = ?error.classify(), line = error.line(), column = error.column(),
                "GitLab CI claim decoding failed");
            "invalid_claims_json"
        }
        ErrorKind::ExpiredSignature => "token_expired",
        ErrorKind::ImmatureSignature => "token_not_yet_valid",
        ErrorKind::InvalidAudience => "invalid_audience",
        ErrorKind::InvalidIssuer => "invalid_issuer",
        ErrorKind::InvalidSignature => "invalid_signature",
        ErrorKind::MissingRequiredClaim(_) => "missing_required_claim",
        _ => "invalid_jwt",
    };
    refused(reason)
}
#[derive(Serialize)]
pub struct TokenResponse {
    pub token: String,
    pub access_token: String,
    pub expires_in: u64,
    pub issued_at: String,
}
impl Auth {
    pub async fn new(config: Config, disk: Disk) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(10));
        if let Some(path) = &config.internal_ca {
            let pem = tokio::fs::read(path).await?;
            for cert in reqwest::Certificate::from_pem_bundle(&pem).map_err(AppError::internal)? {
                builder = builder.add_root_certificate(cert);
            }
        }
        let client = builder.build().map_err(AppError::internal)?;
        let identity =
            serde_json::json!({"audience": config.audience(), "gitlabIssuer": config.issuer()});
        let secret = crate::fs::blocking(move || {
            let _lock = disk.lock("signing-secret", true)?;
            let identity_path = std::path::Path::new("gc/cluster-identity.json");
            match disk.open_read(identity_path) {
                Ok(f) => {
                    let stored: serde_json::Value = serde_json::from_reader(f)?;
                    if stored != identity { return Err(AppError::unavailable("replica audience/GitLab issuer differs from shared storage; explicit operator migration required")); }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => disk.atomic(identity_path, &serde_json::to_vec(&identity)?)?,
                Err(e) => return Err(e.into()),
            }
            let p = std::path::Path::new("gc/signing-secret");
            match disk.open_read(p) {
                Ok(mut file) => {
                    use std::io::Read;
                    let mut bytes = Vec::new();
                    file.read_to_end(&mut bytes)?;
                    if bytes.len() != 32 {
                        return Err(AppError::unavailable("invalid registry signing secret"));
                    }
                    Ok(bytes)
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    let secret: [u8; 32] = rand::random();
                    disk.atomic(p, &secret)?;
                    Ok(secret.to_vec())
                }
                Err(e) => Err(e.into()),
            }
        })
        .await?;
        let auth = Self {
            config,
            client,
            secret: Arc::new(secret),
            keys: Arc::new(Mutex::new(KeyCache {
                keys: JwkSet { keys: Vec::new() },
                fetched: 0,
                attempted: 0,
            })),
        };
        auth.refresh().await?;
        Ok(auth)
    }
    async fn refresh(&self) -> Result<()> {
        let mut cache = self.keys.lock().await;
        let now = crate::now();
        if cache.attempted > 0 && now.saturating_sub(cache.attempted) < 30 {
            return Ok(());
        }
        cache.attempted = now;
        #[derive(Deserialize)]
        struct Discovery {
            issuer: String,
            jwks_uri: url::Url,
        }
        let discovery: Discovery = self
            .fetch(
                self.config
                    .gitlab_issuer
                    .join(".well-known/openid-configuration")
                    .map_err(AppError::internal)?,
            )
            .await?;
        let expected = self
            .config
            .gitlab_issuer
            .join("oauth/discovery/keys")
            .map_err(AppError::internal)?;
        if discovery.issuer != self.config.issuer() || discovery.jwks_uri != expected {
            return Err(AppError::unavailable(
                "GitLab discovery issuer or signing-key URL mismatch",
            ));
        }
        let keys: JwkSet = self.fetch(discovery.jwks_uri).await?;
        if keys.keys.is_empty() {
            return Err(AppError::unavailable("GitLab has no signing keys"));
        }
        cache.keys = keys;
        cache.fetched = now;
        Ok(())
    }
    async fn fetch<T: serde::de::DeserializeOwned>(&self, url: url::Url) -> Result<T> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|_| AppError::unavailable("GitLab discovery is unavailable"))?;
        if !response.status().is_success()
            || response.content_length().is_some_and(|n| n > 1024 * 1024)
        {
            return Err(AppError::unavailable("invalid GitLab discovery response"));
        }
        use futures_util::StreamExt;
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| AppError::unavailable("GitLab discovery read failed"))?;
            if bytes.len() + chunk.len() > 1024 * 1024 {
                return Err(AppError::unavailable("GitLab discovery response too large"));
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes)
            .map_err(|_| AppError::unavailable("invalid GitLab discovery JSON"))
    }
    pub async fn validate_gitlab(&self, token: &str) -> Result<(Actor, u64)> {
        if token.len() > 64 * 1024 {
            return Err(refused("token_too_large"));
        }
        let header = decode_header(token).map_err(|_| refused("invalid_jwt_header"))?;
        if header.alg != Algorithm::RS256 {
            return Err(refused("unsupported_signing_algorithm"));
        }
        let kid = header.kid.ok_or_else(|| refused("missing_kid"))?;
        let refresh = {
            let cache = self.keys.lock().await;
            crate::now().saturating_sub(cache.fetched) > 300 || cache.keys.find(&kid).is_none()
        };
        if refresh && let Err(e) = self.refresh().await {
            tracing::warn!(error=%e, "GitLab key refresh failed");
        }
        let cache = self.keys.lock().await;
        if crate::now().saturating_sub(cache.fetched) > 3600 {
            tracing::info!(
                reason = "signing_key_cache_expired",
                "GitLab CI token refused"
            );
            return Err(AppError::unavailable("GitLab signing-key cache expired"));
        }
        let jwk = cache
            .keys
            .find(&kid)
            .ok_or_else(|| refused("unknown_kid"))?;
        let key = DecodingKey::from_jwk(jwk).map_err(|_| refused("invalid_signing_key"))?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[self.config.issuer()]);
        validation.set_audience(&[self.config.audience()]);
        validation.set_required_spec_claims(&["exp", "iat", "nbf", "iss", "aud", "sub"]);
        validation.leeway = 0;
        validation.validate_nbf = true;
        let claims = decode::<GitlabClaims>(token, &key, &validation)
            .map_err(jwt_refused)?
            .claims;
        let reason = if claims.iat > crate::now() + 30 {
            Some("issued_at_in_future")
        } else if claims.sub.is_empty() {
            Some("empty_subject")
        } else if claims.job_project_id == 0 {
            Some("zero_job_project_id")
        } else if claims.user_access_level < 30 {
            Some("insufficient_access_level")
        } else if !valid_name(&claims.job_project_path) {
            Some("invalid_job_project_path")
        } else if claims.pipeline_source.is_empty() {
            Some("empty_pipeline_source")
        } else if matches!(
            claims.pipeline_source.as_str(),
            "merge_request_event" | "external_pull_request_event"
        ) {
            Some("merge_request_pipeline")
        } else if claims.project_id != claims.job_project_id {
            Some("project_id_mismatch")
        } else if claims.project_path != claims.job_project_path {
            Some("project_path_mismatch")
        } else {
            None
        };
        if let Some(reason) = reason {
            tracing::info!(reason, project_path = %claims.project_path,
                job_project_path = %claims.job_project_path, pipeline_source = %claims.pipeline_source,
                user_access_level = claims.user_access_level, "GitLab CI token refused by policy");
            return Err(AppError::denied());
        }
        Ok((
            Actor {
                owner: Owner {
                    project_id: claims.job_project_id,
                    project_path: claims.job_project_path,
                },
                level: claims.user_access_level,
            },
            claims.exp,
        ))
    }
    pub fn basic(headers: &HeaderMap) -> Result<Option<String>> {
        let Some(header) = headers.get(axum::http::header::AUTHORIZATION) else {
            return Ok(None);
        };
        let header = header
            .to_str()
            .map_err(|_| refused("invalid_basic_credentials"))?;
        let (scheme, value) = header
            .split_once(' ')
            .ok_or_else(|| refused("invalid_basic_credentials"))?;
        if !scheme.eq_ignore_ascii_case("basic") || value.len() > 96 * 1024 {
            return Err(refused("invalid_basic_credentials"));
        }
        let raw = STANDARD
            .decode(value)
            .map_err(|_| refused("invalid_basic_credentials"))?;
        let raw = String::from_utf8(raw).map_err(|_| refused("invalid_basic_credentials"))?;
        let (_, password) = raw
            .split_once(':')
            .ok_or_else(|| refused("invalid_basic_credentials"))?;
        if password.is_empty() {
            return Err(refused("invalid_basic_credentials"));
        }
        Ok(Some(password.to_owned()))
    }
    pub async fn token(&self, headers: &HeaderMap, query: Option<&str>) -> Result<TokenResponse> {
        let pairs: Vec<_> = url::form_urlencoded::parse(query.unwrap_or("").as_bytes())
            .into_owned()
            .collect();
        if pairs.iter().filter(|(k, _)| k == "service").count() != 1
            || !pairs
                .iter()
                .any(|(k, v)| k == "service" && v == &self.config.audience())
        {
            tracing::info!(
                reason = "invalid_token_service",
                "Registry token exchange refused"
            );
            return Err(AppError::bad("UNAUTHORIZED", "invalid token service"));
        }
        let (actor, upstream_exp) = if let Some(token) = Self::basic(headers)? {
            let (actor, exp) = self.validate_gitlab(&token).await?;
            (Some(actor), Some(exp))
        } else {
            (None, None)
        };
        let mut access = Vec::new();
        for (_, scope) in pairs.iter().filter(|(k, _)| k == "scope") {
            for scope in scope.split_whitespace() {
                let parts: Vec<_> = scope.split(':').collect();
                if parts.len() != 3 || parts[0] != "repository" {
                    return Err(AppError::bad("UNAUTHORIZED", "unsupported token scope"));
                }
                check_name(parts[1])?;
                let mut actions = HashSet::new();
                for action in parts[2].split(',') {
                    if action == "pull"
                        || actor.as_ref().is_some_and(|a| {
                            owns(&a.owner, parts[1])
                                && (action == "push" || action == "delete" && a.level >= 40)
                        })
                    {
                        actions.insert(action.to_owned());
                    }
                }
                let mut actions: Vec<_> = actions.into_iter().collect();
                actions.sort();
                access.push(Access {
                    kind: "repository".into(),
                    name: parts[1].into(),
                    actions,
                });
            }
        }
        let now = crate::now();
        let exp = upstream_exp.unwrap_or(now + 300).min(now + 300);
        if exp.saturating_sub(now) < 60 {
            return Err(refused("insufficient_remaining_lifetime"));
        }
        let claims = Claims {
            iss: self.config.audience(),
            aud: self.config.audience(),
            sub: actor
                .as_ref()
                .map(|a| format!("project:{}", a.owner.project_id))
                .unwrap_or_default(),
            exp,
            nbf: now,
            iat: now,
            jti: uuid::Uuid::new_v4().to_string(),
            access,
            actor,
        };
        let token = encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(&self.secret),
        )
        .map_err(AppError::internal)?;
        Ok(TokenResponse {
            access_token: token.clone(),
            token,
            expires_in: exp - now,
            issued_at: rfc3339(now),
        })
    }
    pub fn bearer(&self, headers: &HeaderMap) -> Result<Claims> {
        let header = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(AppError::unauthorized)?;
        let (scheme, token) = header.split_once(' ').ok_or_else(AppError::unauthorized)?;
        if !scheme.eq_ignore_ascii_case("bearer") || token.len() > 96 * 1024 {
            return Err(AppError::unauthorized());
        }
        let mut v = Validation::new(Algorithm::HS256);
        v.set_issuer(&[self.config.audience()]);
        v.set_audience(&[self.config.audience()]);
        v.set_required_spec_claims(&["exp", "nbf", "iat", "iss", "aud"]);
        v.validate_nbf = true;
        v.leeway = 0;
        Ok(
            decode::<Claims>(token, &DecodingKey::from_secret(&self.secret), &v)
                .map_err(|_| AppError::unauthorized())?
                .claims,
        )
    }
    pub fn authorize(&self, headers: &HeaderMap, name: &str, action: &str) -> Result<Owner> {
        let claims = self.bearer(headers)?;
        let actor = claims.actor.ok_or_else(AppError::denied)?;
        if !owns(&actor.owner, name)
            || action == "delete" && actor.level < 40
            || !claims.access.iter().any(|a| {
                a.kind == "repository" && a.name == name && a.actions.iter().any(|v| v == action)
            })
        {
            return Err(AppError::denied());
        }
        Ok(actor.owner)
    }
    pub fn challenge(&self, name: Option<&str>, action: &str) -> String {
        let mut value = format!(
            "Bearer realm=\"{}/auth/token\",service=\"{}\"",
            self.config.audience(),
            self.config.audience()
        );
        if let Some(name) = name {
            value.push_str(&format!(",scope=\"repository:{name}:{action}\""));
        }
        value
    }
}
fn owns(owner: &Owner, name: &str) -> bool {
    name == owner.project_path || name.starts_with(&format!("{}/", owner.project_path))
}
// UTC formatting without a timezone dependency (Gregorian civil date algorithm).
fn rfc3339(seconds: u64) -> String {
    let days = (seconds / 86400) as i64 + 719468;
    let era = days / 146097;
    let doe = days - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds % 86400 / 3600,
        seconds % 3600 / 60,
        seconds % 60
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn date_and_boundaries() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        let o = Owner {
            project_id: 1,
            project_path: "acme/app".into(),
        };
        assert!(owns(&o, "acme/app/worker"));
        assert!(!owns(&o, "acme/application"));
    }
}

#[cfg(test)]
impl Auth {
    pub(crate) fn testing(config: Config, keys: JwkSet) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
            keys: Arc::new(Mutex::new(KeyCache {
                keys,
                fetched: crate::now(),
                attempted: crate::now(),
            })),
            secret: Arc::new(vec![42; 32]),
        }
    }
}
#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    pub fn config() -> Config {
        serde_yaml::from_str("dataRoot: /tmp/registry-test\npublicUrl: https://registry.example.com\ngitlabIssuer: https://gitlab.example.com").unwrap()
    }
    pub fn auth() -> Auth {
        Auth::testing(
            config(),
            serde_json::from_str(include_str!("../tests/fixtures/ci-test-jwks.json")).unwrap(),
        )
    }
    pub fn claims() -> serde_json::Value {
        let now = crate::now();
        serde_json::json!({"iss":"https://gitlab.example.com","aud":"https://registry.example.com","sub":"project_path:team/project:ref_type:branch:ref:main","iat":now,"nbf":now,"exp":now+600,"project_id":"12","project_path":"team/project","job_project_id":"12","job_project_path":"team/project","user_access_level":"maintainer","pipeline_source":"push"})
    }
    pub fn gitlab_job_claims() -> serde_json::Value {
        let now = crate::now();
        serde_json::json!({
            "project_id":"42", "project_path":"acme/widget", "namespace_id":"7",
            "namespace_path":"acme", "user_id":"101", "user_login":"ci-user",
            "user_access_level":"owner", "job_project_id":"42", "job_project_path":"acme/widget",
            "pipeline_id":"1234", "pipeline_source":"push", "job_id":"5678",
            "ref":"master", "ref_type":"branch", "ref_path":"refs/heads/master", "ref_protected":"true",
            "runner_id":9, "runner_environment":"self-hosted", "project_visibility":"internal",
            "iss":"https://gitlab.example.com", "sub":"project_path:acme/widget:ref_type:branch:ref:master",
            "aud":"https://registry.example.com", "iat":now, "nbf":now-5, "exp":now+3600,
            "jti":"public-test-fixture", "future_unused_claim":{"ignored":true}
        })
    }
    pub fn deployment_auth() -> Auth {
        let mut config = config();
        config.public_url = "https://registry.example.com".parse().unwrap();
        config.gitlab_issuer = "https://gitlab.example.com".parse().unwrap();
        Auth::testing(
            config,
            serde_json::from_str(include_str!("../tests/fixtures/ci-test-jwks.json")).unwrap(),
        )
    }
    pub fn sign(value: &serde_json::Value) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("ci-test".into());
        encode(&header, value, &EncodingKey::from_rsa_der(&private_der())).unwrap()
    }
    pub fn private_der() -> Vec<u8> {
        let pem = include_str!("../tests/fixtures/ci-test-private.pem");
        STANDARD
            .decode(
                pem.lines()
                    .filter(|l| !l.starts_with("---"))
                    .collect::<String>(),
            )
            .unwrap()
    }
    pub fn headers(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            format!("Basic {}", STANDARD.encode(format!("anything:{token}")))
                .parse()
                .unwrap(),
        );
        h
    }
}
#[cfg(test)]
mod security_tests {
    use super::fixtures::*;
    use super::*;
    use serde_json::json;
    #[test]
    fn access_level_formats() {
        #[derive(Deserialize)]
        struct Level(#[serde(deserialize_with = "access_level")] u64);
        for (value, expected) in [
            (json!("minimal_access"), 5),
            (json!("guest"), 10),
            (json!("planner"), 15),
            (json!("reporter"), 20),
            (json!("developer"), 30),
            (json!("maintainer"), 40),
            (json!("owner"), 50),
            (json!(30), 30),
            (json!("30"), 30),
        ] {
            assert_eq!(serde_json::from_value::<Level>(value).unwrap().0, expected);
        }
        for value in [
            json!("Owner"),
            json!("administrator"),
            json!(""),
            json!(-1),
            json!(30.5),
            json!(true),
            json!(null),
        ] {
            assert!(serde_json::from_value::<Level>(value).is_err());
        }
    }
    #[tokio::test]
    async fn gitlab_shaped_roles_and_push_permissions() {
        let auth = deployment_auth();
        for (role, level) in [
            (json!("owner"), 50),
            (json!("developer"), 30),
            (json!(30), 30),
            (json!("30"), 30),
        ] {
            let mut c = gitlab_job_claims();
            c["user_access_level"] = role;
            let (actor, _) = auth.validate_gitlab(&sign(&c)).await.unwrap();
            assert_eq!(actor.level, level);
            assert_eq!(actor.owner.project_id, 42);
            let result = auth.token(&headers(&sign(&c)), Some("service=https%3A%2F%2Fregistry.example.com&scope=repository:acme/widget:push,pull")).await.unwrap();
            let mut bearer = HeaderMap::new();
            bearer.insert(
                axum::http::header::AUTHORIZATION,
                format!("Bearer {}", result.token).parse().unwrap(),
            );
            assert!(auth.authorize(&bearer, "acme/widget", "push").is_ok());
            assert_eq!(
                auth.authorize(&bearer, "acme/other", "push")
                    .unwrap_err()
                    .status,
                axum::http::StatusCode::FORBIDDEN
            );
            assert_eq!(
                auth.authorize(&bearer, "acme/widget", "delete")
                    .unwrap_err()
                    .status,
                axum::http::StatusCode::FORBIDDEN
            );
        }
        for (field, value, status) in [
            (
                "user_access_level",
                json!("reporter"),
                axum::http::StatusCode::FORBIDDEN,
            ),
            (
                "user_access_level",
                json!("unknown"),
                axum::http::StatusCode::UNAUTHORIZED,
            ),
            (
                "user_access_level",
                json!("Owner"),
                axum::http::StatusCode::UNAUTHORIZED,
            ),
            (
                "pipeline_source",
                json!("merge_request_event"),
                axum::http::StatusCode::FORBIDDEN,
            ),
        ] {
            let mut c = gitlab_job_claims();
            c[field] = value;
            assert_eq!(
                auth.validate_gitlab(&sign(&c)).await.unwrap_err().status,
                status
            );
        }
    }
    #[tokio::test]
    async fn refusal_logs_are_useful_and_do_not_contain_credentials() {
        // Isolate tracing's global callsite-interest cache from concurrent tests
        // running without a subscriber; otherwise scoped capture can miss events.
        if std::env::var_os("RORO_LOG_TEST_CHILD").is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "auth::security_tests::refusal_logs_are_useful_and_do_not_contain_credentials",
                ])
                .env("RORO_LOG_TEST_CHILD", "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        use std::io::Write;
        use tracing::instrument::WithSubscriber;
        #[derive(Clone)]
        struct Logs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl Write for Logs {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let logs = Logs(Default::default());
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let mut secrets = Vec::new();
        async {
            let auth = deployment_auth();
            for (field, value) in [
                ("user_access_level", json!("reporter")),
                ("user_access_level", json!("unknown")),
                ("pipeline_source", json!("merge_request_event")),
                ("aud", json!("https://wrong.example")),
            ] {
                let mut c = gitlab_job_claims();
                c[field] = value;
                let token = sign(&c);
                assert!(auth.validate_gitlab(&token).await.is_err());
                secrets.push(token);
            }
            for kid in [None, Some("unknown")] {
                let mut header = Header::new(Algorithm::RS256);
                header.kid = kid.map(String::from);
                let token = encode(
                    &header,
                    &gitlab_job_claims(),
                    &EncodingKey::from_rsa_der(&private_der()),
                )
                .unwrap();
                assert!(auth.validate_gitlab(&token).await.is_err());
                secrets.push(token);
            }
            let mut c = gitlab_job_claims();
            c["exp"] = json!(crate::now() + 30);
            let token = sign(&c);
            assert!(
                auth.token(
                    &headers(&token),
                    Some("service=https%3A%2F%2Fregistry.example.com")
                )
                .await
                .is_err()
            );
            secrets.push(token);
            // A malicious value must not be echoed by serde's diagnostic formatter.
            let mut c = gitlab_job_claims();
            c["iat"] = json!("secret-claim-marker");
            assert!(auth.validate_gitlab(&sign(&c)).await.is_err());
        }
        .with_subscriber(subscriber)
        .await;
        let output = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        for reason in [
            "insufficient_access_level",
            "invalid_claims_json",
            "invalid_user_access_level",
            "GitLab CI claim decoding failed",
            "merge_request_pipeline",
            "invalid_audience",
            "missing_kid",
            "unknown_kid",
            "insufficient_remaining_lifetime",
        ] {
            assert!(output.contains(reason), "missing {reason}: {output}");
        }
        assert!(!output.contains("secret-claim-marker"));
        for token in secrets {
            assert!(!output.contains(&token));
            assert!(!output.contains(token.rsplit('.').next().unwrap()));
            assert!(!output.contains(&STANDARD.encode(format!("anything:{token}"))));
        }
    }
    #[tokio::test]
    async fn validates_signed_project_identity() {
        let auth = auth();
        let (actor, _) = auth.validate_gitlab(&sign(&claims())).await.unwrap();
        assert_eq!(actor.owner.project_id, 12);
        assert_eq!(actor.level, 40);
    }
    #[tokio::test]
    async fn rejects_invalid_claims() {
        let auth = auth();
        for (field, value) in [
            ("iss", serde_json::json!("https://evil.example")),
            ("aud", serde_json::json!("https://other.example")),
            ("exp", serde_json::json!(crate::now() - 1)),
            ("iat", serde_json::json!(crate::now() + 600)),
            ("nbf", serde_json::json!(crate::now() + 600)),
            ("pipeline_source", serde_json::json!("merge_request_event")),
            ("user_access_level", serde_json::json!("20")),
            ("project_id", serde_json::json!("99")),
            ("job_project_path", serde_json::json!("../escape")),
        ] {
            let mut c = claims();
            c[field] = value;
            assert!(
                auth.validate_gitlab(&sign(&c)).await.is_err(),
                "accepted invalid {field}"
            );
        }
        for field in [
            "exp",
            "iat",
            "nbf",
            "job_project_id",
            "job_project_path",
            "user_access_level",
            "pipeline_source",
        ] {
            let mut c = claims();
            c.as_object_mut().unwrap().remove(field);
            assert!(
                auth.validate_gitlab(&sign(&c)).await.is_err(),
                "accepted missing {field}"
            );
        }
    }
    #[tokio::test]
    async fn rejects_wrong_key_algorithm_and_unknown_key() {
        let auth = auth();
        let mut c = claims();
        let forged = encode(
            &Header::new(Algorithm::HS256),
            &c,
            &EncodingKey::from_secret(b"not-a-key"),
        )
        .unwrap();
        assert!(auth.validate_gitlab(&forged).await.is_err());
        let mut h = Header::new(Algorithm::RS256);
        h.kid = Some("unknown".into());
        let t = encode(&h, &c, &EncodingKey::from_rsa_der(&private_der())).unwrap();
        assert!(auth.validate_gitlab(&t).await.is_err());
        let token = sign(&c);
        c["project_id"] = serde_json::json!(99);
        let fake = sign(&c);
        let forged = format!(
            "{}.{}.{}",
            token.split('.').next().unwrap(),
            fake.split('.').nth(1).unwrap(),
            token.split('.').nth(2).unwrap()
        );
        assert!(auth.validate_gitlab(&forged).await.is_err());
    }
    #[tokio::test]
    async fn scopes_cannot_escalate() {
        let auth = auth();
        let h = headers(&sign(&claims()));
        let result=auth.token(&h,Some("service=https%3A%2F%2Fregistry.example.com&scope=repository:other/project:pull,push,delete&scope=repository:team/project:pull,push,delete")).await.unwrap();
        let mut bearer = HeaderMap::new();
        bearer.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", result.token).parse().unwrap(),
        );
        assert!(auth.authorize(&bearer, "team/project", "push").is_ok());
        assert!(auth.authorize(&bearer, "team/project", "delete").is_ok());
        assert!(auth.authorize(&bearer, "other/project", "push").is_err());
        assert!(
            auth.authorize(&bearer, "team/project/child", "push")
                .is_err()
        );
        let anonymous=auth.token(&HeaderMap::new(),Some("service=https%3A%2F%2Fregistry.example.com&scope=repository:team/project:pull,push")).await.unwrap();
        bearer.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", anonymous.token).parse().unwrap(),
        );
        assert!(auth.authorize(&bearer, "team/project", "push").is_err());
        assert_eq!(
            auth.bearer(&bearer).unwrap().access[0].actions,
            vec!["pull"]
        );
    }
    #[tokio::test]
    async fn stale_cache_and_rotation() {
        let auth = auth();
        let token = sign(&claims());
        assert!(auth.validate_gitlab(&token).await.is_ok());
        {
            let mut cache = auth.keys.lock().await;
            cache.fetched = crate::now() - 3601;
            cache.attempted = crate::now();
        }
        assert!(auth.validate_gitlab(&token).await.is_err());
        {
            let mut cache = auth.keys.lock().await;
            cache.fetched = crate::now();
            cache.keys.keys.clear();
        }
        assert!(auth.validate_gitlab(&token).await.is_err());
        {
            let mut cache = auth.keys.lock().await;
            cache.keys =
                serde_json::from_str(include_str!("../tests/fixtures/ci-test-jwks.json")).unwrap();
        }
        assert!(auth.validate_gitlab(&token).await.is_ok());
    }
    #[tokio::test]
    async fn near_expiry_and_invalid_credentials() {
        let auth = auth();
        let mut c = claims();
        c["exp"] = serde_json::json!(crate::now() + 30);
        assert!(
            auth.token(
                &headers(&sign(&c)),
                Some("service=https%3A%2F%2Fregistry.example.com")
            )
            .await
            .is_err()
        );
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            "Basic not-base64".parse().unwrap(),
        );
        assert!(Auth::basic(&h).is_err());
    }
}
