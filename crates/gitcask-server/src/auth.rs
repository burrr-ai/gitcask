//! Stateless request authentication: local Ed25519 JWT verification or trusted
//! forwarded headers. Identity remains opaque; repository scopes are carried by
//! the token and checked by the handlers' existing read/write/admin gates.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail, ensure};
use axum::http::HeaderMap;
use base64::Engine as _;
use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;
use ed25519_dalek::pkcs8::{
    DecodePrivateKey as _, DecodePublicKey as _, EncodePrivateKey as _, EncodePublicKey as _,
};
use ed25519_dalek::{Signature, Signer as _, SigningKey, Verifier as _, VerifyingKey};
use futures::StreamExt as _;
use gitcask_config::{AuthMode, JwtConfig};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq as _;

const PRINCIPAL_HEADER: &str = "x-gitcask-principal";
const WRITE_HEADER: &str = "x-gitcask-write";
const ADMIN_HEADER: &str = "x-gitcask-admin";
const FORWARD_SECRET_HEADER: &str = "x-gitcask-forward-secret";
const FORWARD_SECRET_ENV: &str = "GITCASK_FORWARD_SECRET";
const MAX_JWKS_BYTES: usize = 1024 * 1024;

/// A resolved principal. The name is opaque to gitcask.
#[derive(Debug, Clone)]
pub struct Principal {
    pub name: String,
    pub write: bool,
    /// Repository deletion, independent of `write` in forwarded mode.
    pub admin: bool,
    pub anonymous: bool,
    scopes: Option<Vec<Scope>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Permission {
    Read,
    Write,
    Admin,
}

impl Permission {
    fn parse(value: &str) -> std::result::Result<Self, String> {
        match value {
            "read" => Ok(Self::Read),
            "write" => Ok(Self::Write),
            "admin" => Ok(Self::Admin),
            _ => Err(format!("unknown permission {value:?}")),
        }
    }

    fn allows(self, required: Self) -> bool {
        self >= required
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Repository {
    owner: String,
    name: String,
}

impl Repository {
    fn new(owner: &str, name: &str) -> Option<Self> {
        if !valid_component(owner, false) || !valid_component(name, false) {
            return None;
        }
        let name = name.to_ascii_lowercase();
        let name = name.strip_suffix(".git").unwrap_or(&name);
        if name.is_empty() {
            return None;
        }
        Some(Self {
            owner: owner.to_ascii_lowercase(),
            name: name.to_string(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Scope {
    owner: String,
    repository_pattern: String,
    permission: Permission,
}

impl Scope {
    fn parse(value: &str) -> std::result::Result<Self, String> {
        let (repository, permission) = value
            .rsplit_once(':')
            .ok_or_else(|| format!("scope must be <owner>/<repo>:<permission>: {value:?}"))?;
        let (owner, repository_pattern) = repository
            .split_once('/')
            .ok_or_else(|| format!("scope must contain one owner/repository pair: {value:?}"))?;
        if owner.is_empty()
            || repository_pattern.is_empty()
            || repository_pattern.contains('/')
            || owner.contains('*')
            || !valid_component(owner, false)
            || !valid_component(repository_pattern, true)
        {
            return Err(format!("invalid repository scope {value:?}"));
        }
        Ok(Self {
            owner: owner.to_ascii_lowercase(),
            repository_pattern: repository_pattern.to_ascii_lowercase(),
            permission: Permission::parse(permission)?,
        })
    }

    fn grants(&self, repository: &Repository) -> Option<Permission> {
        (self.owner == repository.owner && glob_matches(&self.repository_pattern, &repository.name))
            .then_some(self.permission)
    }
}

fn valid_component(value: &str, allow_glob: bool) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'_' | b'-')
                || (allow_glob && byte == b'*')
        })
}

fn glob_matches(pattern: &str, value: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == value;
    }
    let anchored_start = !pattern.starts_with('*');
    let anchored_end = !pattern.ends_with('*');
    let mut parts = pattern
        .split('*')
        .filter(|part| !part.is_empty())
        .peekable();
    let mut remaining = value;
    let mut first = true;
    while let Some(part) = parts.next() {
        let last = parts.peek().is_none();
        if first && anchored_start {
            let Some(rest) = remaining.strip_prefix(part) else {
                return false;
            };
            remaining = rest;
        } else if last && anchored_end {
            return remaining.ends_with(part);
        } else {
            let Some(position) = remaining.find(part) else {
                return false;
            };
            let Some(rest) = remaining.get(position + part.len()..) else {
                return false;
            };
            remaining = rest;
        }
        first = false;
    }
    !anchored_end || remaining.is_empty()
}

impl Principal {
    fn permission_for(&self, repository: &Repository) -> Option<Permission> {
        self.scopes
            .as_ref()?
            .iter()
            .filter_map(|scope| scope.grants(repository))
            .max()
    }
}

enum Backend {
    None,
    Forwarded { forward_secret: Option<String> },
    Jwt(Box<JwtVerifier>),
}

/// Pluggable authenticator backed by [`gitcask_config::AuthMode`].
pub struct Authenticator {
    backend: Backend,
}

impl Authenticator {
    pub async fn new(cfg: &gitcask_config::Config) -> Result<Arc<Self>> {
        let backend = match cfg.server.auth_mode {
            AuthMode::None => Backend::None,
            AuthMode::Forwarded => Backend::Forwarded {
                forward_secret: std::env::var(FORWARD_SECRET_ENV)
                    .ok()
                    .filter(|value| !value.is_empty()),
            },
            AuthMode::Jwt => Backend::Jwt(Box::new(JwtVerifier::new(&cfg.auth.jwt).await?)),
        };
        Ok(Arc::new(Self { backend }))
    }

    /// Verify a principal without applying a repository permission. Used only
    /// by non-repository routes and middleware.
    pub async fn authenticate(&self, headers: &HeaderMap) -> Result<Principal, AuthError> {
        let principal = match &self.backend {
            Backend::None => Principal {
                name: "anon".to_string(),
                write: true,
                admin: true,
                anonymous: true,
                scopes: None,
            },
            Backend::Forwarded { forward_secret } => forwarded(headers, forward_secret.as_deref())?,
            Backend::Jwt(verifier) => {
                let credential = client_credential(headers).ok_or(AuthError::Unauthorized)?;
                verifier.verify(&credential).await.map_err(|error| {
                    tracing::debug!(%error, "JWT authentication failed");
                    AuthError::Unauthorized
                })?
            }
        };
        tracing::Span::current().record("principal", principal.name.as_str());
        Ok(principal)
    }

    async fn require_permission(
        &self,
        headers: &HeaderMap,
        owner: &str,
        repo: &str,
        required: Permission,
    ) -> Result<Principal, AuthError> {
        let mut principal = self.authenticate(headers).await?;
        if matches!(&self.backend, Backend::Jwt(_)) {
            let repository = Repository::new(owner, repo).ok_or(AuthError::NotFound)?;
            let granted = principal
                .permission_for(&repository)
                .filter(|permission| permission.allows(required))
                .ok_or(AuthError::NotFound)?;
            principal.write = granted.allows(Permission::Write);
            principal.admin = granted.allows(Permission::Admin);
            return Ok(principal);
        }
        let allowed = match required {
            Permission::Read => true,
            Permission::Write => principal.write,
            Permission::Admin => principal.admin,
        };
        if allowed {
            Ok(principal)
        } else {
            Err(AuthError::Forbidden)
        }
    }

    /// Require `write` for git push, LFS upload, repository creation, and write APIs.
    pub async fn require_write(
        &self,
        headers: &HeaderMap,
        owner: &str,
        repo: &str,
    ) -> Result<Principal, AuthError> {
        self.require_permission(headers, owner, repo, Permission::Write)
            .await
    }

    /// Require permission to delete a repository.
    pub async fn require_admin(
        &self,
        headers: &HeaderMap,
        owner: &str,
        repo: &str,
    ) -> Result<Principal, AuthError> {
        self.require_permission(headers, owner, repo, Permission::Admin)
            .await
    }

    /// Require read access to one repository.
    pub async fn require_read(
        &self,
        headers: &HeaderMap,
        owner: &str,
        repo: &str,
    ) -> Result<Principal, AuthError> {
        self.require_permission(headers, owner, repo, Permission::Read)
            .await
    }
}

fn forwarded(headers: &HeaderMap, forward_secret: Option<&str>) -> Result<Principal, AuthError> {
    if let Some(expected) = forward_secret {
        let presented = header_value(headers, FORWARD_SECRET_HEADER);
        let matches =
            presented.is_some_and(|value| bool::from(value.as_bytes().ct_eq(expected.as_bytes())));
        if !matches {
            return Err(AuthError::Unauthorized);
        }
    }
    let name = header_value(headers, PRINCIPAL_HEADER).ok_or(AuthError::Unauthorized)?;
    Ok(Principal {
        name: name.to_string(),
        write: header_value(headers, WRITE_HEADER) == Some("1"),
        admin: header_value(headers, ADMIN_HEADER) == Some("1"),
        anonymous: false,
        scopes: None,
    })
}

struct JwtVerifier {
    issuer: String,
    audience: Option<String>,
    leeway: Duration,
    keys: VerificationKeys,
}

enum VerificationKeys {
    Public(VerifyingKey),
    Jwks(JwksKeys),
}

impl JwtVerifier {
    async fn new(config: &JwtConfig) -> Result<Self> {
        ensure!(!config.issuer.trim().is_empty(), "auth.jwt.issuer is empty");
        let keys = match (&config.public_key, &config.jwks_url) {
            (Some(public_key), None) => {
                let pem = if public_key.contains("-----BEGIN PUBLIC KEY-----") {
                    public_key.clone()
                } else {
                    tokio::fs::read_to_string(public_key)
                        .await
                        .with_context(|| format!("reading Ed25519 public key {public_key}"))?
                };
                VerificationKeys::Public(
                    VerifyingKey::from_public_key_pem(&pem)
                        .context("parsing auth.jwt.public_key as Ed25519 PEM")?,
                )
            }
            (None, Some(url)) => VerificationKeys::Jwks(JwksKeys::new(url)?),
            _ => bail!("jwt mode requires exactly one public key source"),
        };
        Ok(Self {
            issuer: config.issuer.clone(),
            audience: config.audience.clone(),
            leeway: config.leeway,
            keys,
        })
    }

    async fn verify(&self, token: &str) -> Result<Principal> {
        let parsed = ParsedToken::parse(token)?;
        let key = match &self.keys {
            VerificationKeys::Public(key) => *key,
            VerificationKeys::Jwks(keys) => {
                let kid = parsed
                    .header
                    .kid
                    .as_deref()
                    .filter(|kid| !kid.is_empty())
                    .context("JWKS token has no kid")?;
                keys.key(kid).await.context("JWT kid is not in the JWKS")?
            }
        };
        parsed.verify(&key, &self.issuer, self.audience.as_deref(), self.leeway)
    }
}

struct JwksKeys {
    url: String,
    client: reqwest::Client,
    cache: tokio::sync::RwLock<HashMap<String, VerifyingKey>>,
    refresh: tokio::sync::Mutex<()>,
}

impl JwksKeys {
    fn new(url: &str) -> Result<Self> {
        Ok(Self {
            url: url.to_string(),
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(10))
                .build()
                .context("building JWKS client")?,
            cache: tokio::sync::RwLock::new(HashMap::new()),
            refresh: tokio::sync::Mutex::new(()),
        })
    }

    async fn key(&self, kid: &str) -> Option<VerifyingKey> {
        if let Some(key) = self.cache.read().await.get(kid).copied() {
            return Some(key);
        }
        let _refresh = self.refresh.lock().await;
        if let Some(key) = self.cache.read().await.get(kid).copied() {
            return Some(key);
        }
        match self.fetch().await {
            Ok(keys) => {
                *self.cache.write().await = keys;
            }
            Err(error) => {
                // Keep the last successful set. Existing kids never enter this path,
                // so an issuer outage cannot break already-known signing keys.
                tracing::warn!(url = %self.url, %error, "JWKS refresh failed; retaining cached keys");
            }
        }
        self.cache.read().await.get(kid).copied()
    }

    async fn fetch(&self) -> Result<HashMap<String, VerifyingKey>> {
        let response = self
            .client
            .get(&self.url)
            .send()
            .await
            .context("fetching JWKS")?
            .error_for_status()
            .context("JWKS endpoint returned an error")?;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_JWKS_BYTES as u64)
        {
            bail!("JWKS exceeds {MAX_JWKS_BYTES} bytes");
        }
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("reading JWKS")?;
            ensure!(
                body.len().saturating_add(chunk.len()) <= MAX_JWKS_BYTES,
                "JWKS exceeds {MAX_JWKS_BYTES} bytes"
            );
            body.extend_from_slice(&chunk);
        }
        let jwks: JwkSet = serde_json::from_slice(&body).context("parsing JWKS")?;
        let mut keys = HashMap::new();
        for jwk in jwks.keys {
            if jwk.kty != "OKP" || jwk.crv != "Ed25519" {
                continue;
            }
            if jwk.alg.as_deref().is_some_and(|alg| alg != "EdDSA")
                || jwk.use_.as_deref().is_some_and(|use_| use_ != "sig")
            {
                continue;
            }
            let kid = jwk
                .kid
                .filter(|kid| !kid.is_empty())
                .context("Ed25519 JWK has no kid")?;
            let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(jwk.x)
                .context("decoding Ed25519 JWK x")?;
            let bytes: [u8; 32] = decoded
                .try_into()
                .map_err(|_| anyhow::anyhow!("Ed25519 JWK x must be 32 bytes"))?;
            let key = VerifyingKey::from_bytes(&bytes).context("invalid Ed25519 JWK")?;
            ensure!(keys.insert(kid, key).is_none(), "duplicate JWKS kid");
        }
        ensure!(!keys.is_empty(), "JWKS contains no Ed25519 signing keys");
        Ok(keys)
    }
}

#[derive(Deserialize)]
struct JwkSet {
    keys: Vec<Jwk>,
}

#[derive(Deserialize)]
struct Jwk {
    kty: String,
    crv: String,
    x: String,
    kid: Option<String>,
    alg: Option<String>,
    #[serde(rename = "use")]
    use_: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    iss: String,
    sub: String,
    scopes: Vec<String>,
    exp: u64,
    iat: u64,
    jti: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    aud: Option<Audience>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    nbf: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

impl Audience {
    fn contains(&self, expected: &str) -> bool {
        match self {
            Self::One(value) => value == expected,
            Self::Many(values) => values.iter().any(|value| value == expected),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct JwtHeader {
    alg: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    typ: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kid: Option<String>,
}

struct ParsedToken {
    header: JwtHeader,
    claims_segment: String,
    signing_input: String,
    signature: Signature,
}

impl ParsedToken {
    fn parse(token: &str) -> Result<Self> {
        ensure!(token.len() <= 64 * 1024, "JWT is too large");
        let mut segments = token.split('.');
        let (Some(header_segment), Some(claims_segment), Some(signature_segment), None) = (
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
        ) else {
            bail!("JWT must have three segments");
        };
        let header_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(header_segment)
            .context("decoding JWT header")?;
        let header: JwtHeader =
            serde_json::from_slice(&header_bytes).context("parsing JWT header")?;
        ensure!(header.alg == "EdDSA", "JWT alg must be EdDSA");
        ensure!(
            header.typ.as_deref().is_none_or(|typ| typ == "JWT"),
            "unsupported JWT typ"
        );
        let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(signature_segment)
            .context("decoding JWT signature")?;
        let signature = Signature::from_slice(&signature).context("invalid Ed25519 signature")?;
        Ok(Self {
            header,
            claims_segment: claims_segment.to_string(),
            signing_input: format!("{header_segment}.{claims_segment}"),
            signature,
        })
    }

    fn verify(
        self,
        key: &VerifyingKey,
        issuer: &str,
        audience: Option<&str>,
        leeway: Duration,
    ) -> Result<Principal> {
        key.verify(self.signing_input.as_bytes(), &self.signature)
            .context("invalid JWT signature")?;
        let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(self.claims_segment)
            .context("decoding JWT claims")?;
        let claims: Claims = serde_json::from_slice(&claims).context("parsing JWT claims")?;
        let now = unix_timestamp()?;
        let leeway = leeway.as_secs();
        ensure!(claims.iss == issuer, "JWT issuer mismatch");
        if let Some(expected) = audience {
            ensure!(
                claims
                    .aud
                    .as_ref()
                    .is_some_and(|aud| aud.contains(expected)),
                "JWT audience mismatch"
            );
        }
        ensure!(claims.exp > claims.iat, "invalid JWT times");
        ensure!(now < claims.exp.saturating_add(leeway), "JWT expired");
        ensure!(
            claims.iat <= now.saturating_add(leeway),
            "JWT issued in the future"
        );
        ensure!(
            claims
                .nbf
                .is_none_or(|not_before| not_before <= now.saturating_add(leeway)),
            "JWT is not active yet"
        );
        ensure!(
            !claims.sub.is_empty() && !claims.jti.is_empty(),
            "invalid JWT identity"
        );
        let scopes = parse_scopes(&claims.scopes)?;
        Ok(Principal {
            name: claims.sub,
            write: false,
            admin: false,
            anonymous: false,
            scopes: Some(scopes),
        })
    }
}

fn parse_scopes(scopes: &[String]) -> Result<Vec<Scope>> {
    scopes
        .iter()
        .map(|scope| Scope::parse(scope).map_err(anyhow::Error::msg))
        .collect()
}

/// Mint the token format accepted by `auth_mode = "jwt"`. The server never
/// calls this; it is exposed for the offline `gitcask token mint` CLI.
pub fn mint_token(
    private_key_pem: &str,
    issuer: &str,
    audience: Option<&str>,
    principal: &str,
    scopes: &[String],
    ttl: Duration,
) -> Result<String> {
    ensure!(!issuer.trim().is_empty(), "issuer may not be empty");
    ensure!(!principal.is_empty(), "principal may not be empty");
    ensure!(!scopes.is_empty(), "at least one scope is required");
    parse_scopes(scopes)?;
    ensure!(!ttl.is_zero(), "ttl must be greater than zero");
    let issued_at = unix_timestamp()?;
    let expires_at = issued_at
        .checked_add(ttl.as_secs())
        .context("token expiration overflow")?;
    let claims = Claims {
        iss: issuer.to_string(),
        sub: principal.to_string(),
        scopes: scopes.to_vec(),
        exp: expires_at,
        iat: issued_at,
        jti: uuid::Uuid::new_v4().to_string(),
        aud: audience.map(|value| Audience::One(value.to_string())),
        nbf: None,
    };
    let key = SigningKey::from_pkcs8_pem(private_key_pem)
        .context("parsing Ed25519 private key as PKCS#8 PEM")?;
    sign_claims(&claims, &key, None, "EdDSA")
}

fn sign_claims(
    claims: &Claims,
    key: &SigningKey,
    kid: Option<&str>,
    algorithm: &str,
) -> Result<String> {
    let header = JwtHeader {
        alg: algorithm.to_string(),
        typ: Some("JWT".to_string()),
        kid: kid.map(str::to_string),
    };
    let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&header).context("serializing JWT header")?);
    let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(claims).context("serializing JWT claims")?);
    let signing_input = format!("{header}.{claims}");
    let signature = key.sign(signing_input.as_bytes());
    let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.to_bytes());
    Ok(format!("{signing_input}.{signature}"))
}

/// Generate a PKCS#8 Ed25519 private key and matching public-key PEM.
pub fn generate_key_pair_pem() -> Result<(String, String)> {
    use rand::RngCore as _;
    let mut secret = [0_u8; 32];
    rand::rng().fill_bytes(&mut secret);
    let signing = SigningKey::from_bytes(&secret);
    let private = signing
        .to_pkcs8_pem(LineEnding::LF)
        .context("encoding Ed25519 private key")?
        .as_str()
        .to_string();
    let public = signing
        .verifying_key()
        .to_public_key_pem(LineEnding::LF)
        .context("encoding Ed25519 public key")?;
    Ok((private, public))
}

fn unix_timestamp() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_secs())
}

fn client_credential(headers: &HeaderMap) -> Option<String> {
    if let Some(token) = bearer_credential(headers) {
        return Some(token.to_string());
    }
    let value = headers.get(http::header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, encoded) = value.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (_, password) = decoded.split_once(':')?;
    (!password.is_empty()).then_some(password.to_string())
}

fn bearer_credential(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(http::header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.trim().split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty()).then(|| token.trim())
}

fn header_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    Unauthorized,
    Forbidden,
    NotFound,
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use axum::Router;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::routing::get;
    use base64::engine::general_purpose::STANDARD;

    use super::*;

    fn bearer(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            format!("Bearer {token}").parse().expect("header"),
        );
        headers
    }

    fn claims(now: u64) -> Claims {
        Claims {
            iss: "https://issuer.example".into(),
            sub: "opaque:alice".into(),
            scopes: vec!["acme/app:admin".into()],
            exp: now + 60,
            iat: now,
            jti: "test-jti".into(),
            aud: Some(Audience::One("gitcask".into())),
            nbf: None,
        }
    }

    async fn public_key_auth(public: String, leeway: Duration) -> Arc<Authenticator> {
        let mut config = gitcask_config::Config::default();
        config.server.auth_mode = AuthMode::Jwt;
        config.auth.jwt = JwtConfig {
            public_key: Some(public),
            issuer: "https://issuer.example".into(),
            audience: Some("gitcask".into()),
            leeway,
            ..JwtConfig::default()
        };
        Authenticator::new(&config)
            .await
            .expect("JWT authenticator")
    }

    #[tokio::test]
    async fn jwt_scopes_normalize_glob_and_imply_permissions() {
        let (private, public) = generate_key_pair_pem().expect("keys");
        let auth = public_key_auth(public, Duration::ZERO).await;
        let token = mint_token(
            &private,
            "https://issuer.example",
            Some("gitcask"),
            "opaque:alice",
            &["ACME/App-*:admin".into()],
            Duration::from_mins(1),
        )
        .expect("token");
        let headers = bearer(&token);
        assert!(
            auth.require_read(&headers, "acme", "APP-one.GIT")
                .await
                .is_ok()
        );
        assert!(
            auth.require_write(&headers, "ACME", "app-one")
                .await
                .is_ok()
        );
        assert!(
            auth.require_admin(&headers, "acme", "app-one")
                .await
                .is_ok()
        );
        assert_eq!(
            auth.require_read(&headers, "other", "app-one")
                .await
                .unwrap_err(),
            AuthError::NotFound
        );
        assert!(Scope::parse("*/app:read").is_err());
        assert!(Scope::parse("acme/team/app:read").is_err());
    }

    #[tokio::test]
    async fn basic_password_is_the_git_credential_and_spoofed_headers_are_ignored() {
        let (private, public) = generate_key_pair_pem().expect("keys");
        let auth = public_key_auth(public, Duration::ZERO).await;
        let token = mint_token(
            &private,
            "https://issuer.example",
            Some("gitcask"),
            "alice",
            &["acme/app:read".into()],
            Duration::from_mins(1),
        )
        .expect("token");
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            format!("Basic {}", STANDARD.encode(format!("ignored:{token}")))
                .parse()
                .expect("header"),
        );
        headers.insert(PRINCIPAL_HEADER, "spoofed".parse().expect("header"));
        headers.insert(WRITE_HEADER, "1".parse().expect("header"));
        headers.insert(ADMIN_HEADER, "1".parse().expect("header"));
        assert_eq!(
            auth.require_write(&headers, "acme", "app")
                .await
                .unwrap_err(),
            AuthError::NotFound
        );
        assert_eq!(
            auth.require_read(&headers, "acme", "app")
                .await
                .expect("read")
                .name,
            "alice"
        );
    }

    #[tokio::test]
    async fn expiry_nbf_wrong_signature_and_algorithm_confusion_are_rejected() {
        let (private, public) = generate_key_pair_pem().expect("keys");
        let (wrong_private, _) = generate_key_pair_pem().expect("wrong keys");
        let auth = public_key_auth(public, Duration::ZERO).await;
        let key = SigningKey::from_pkcs8_pem(&private).expect("private key");
        let wrong_key = SigningKey::from_pkcs8_pem(&wrong_private).expect("wrong private key");
        let now = unix_timestamp().expect("clock");

        let mut expired = claims(now);
        expired.iat = now - 20;
        expired.exp = now - 10;
        let expired = sign_claims(&expired, &key, None, "EdDSA").expect("expired token");
        assert_eq!(
            auth.authenticate(&bearer(&expired)).await.unwrap_err(),
            AuthError::Unauthorized
        );

        let mut future = claims(now);
        future.nbf = Some(now + 30);
        let future = sign_claims(&future, &key, None, "EdDSA").expect("future token");
        assert_eq!(
            auth.authenticate(&bearer(&future)).await.unwrap_err(),
            AuthError::Unauthorized
        );

        let wrong = sign_claims(&claims(now), &wrong_key, None, "EdDSA").expect("wrong token");
        assert_eq!(
            auth.authenticate(&bearer(&wrong)).await.unwrap_err(),
            AuthError::Unauthorized
        );

        let none = sign_claims(&claims(now), &key, None, "none").expect("none token");
        assert_eq!(
            auth.authenticate(&bearer(&none)).await.unwrap_err(),
            AuthError::Unauthorized
        );
    }

    struct JwksState {
        body: String,
        calls: AtomicUsize,
        fail: AtomicBool,
    }

    async fn jwks(State(state): State<Arc<JwksState>>) -> impl axum::response::IntoResponse {
        state.calls.fetch_add(1, Ordering::SeqCst);
        if state.fail.load(Ordering::SeqCst) {
            return (StatusCode::SERVICE_UNAVAILABLE, "unavailable".to_string());
        }
        (StatusCode::OK, state.body.clone())
    }

    #[tokio::test]
    async fn jwks_fetches_only_on_kid_miss_and_keeps_the_last_success() {
        let (private, _) = generate_key_pair_pem().expect("keys");
        let signing = SigningKey::from_pkcs8_pem(&private).expect("private key");
        let x = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(signing.verifying_key().as_bytes());
        let state = Arc::new(JwksState {
            body: serde_json::json!({
                "keys": [{"kty":"OKP", "crv":"Ed25519", "x":x, "kid":"k1", "alg":"EdDSA", "use":"sig"}]
            })
            .to_string(),
            calls: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let app = Router::new()
            .route("/jwks", get(jwks))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut config = gitcask_config::Config::default();
        config.server.auth_mode = AuthMode::Jwt;
        config.auth.jwt = JwtConfig {
            jwks_url: Some(format!("http://{address}/jwks")),
            issuer: "https://issuer.example".into(),
            audience: Some("gitcask".into()),
            leeway: Duration::ZERO,
            ..JwtConfig::default()
        };
        let auth = Authenticator::new(&config).await.expect("auth");
        let now = unix_timestamp().expect("clock");
        let token = sign_claims(&claims(now), &signing, Some("k1"), "EdDSA").expect("token");
        assert!(
            auth.require_read(&bearer(&token), "acme", "app")
                .await
                .is_ok()
        );
        assert!(
            auth.require_read(&bearer(&token), "acme", "app")
                .await
                .is_ok()
        );
        assert_eq!(state.calls.load(Ordering::SeqCst), 1);

        state.fail.store(true, Ordering::SeqCst);
        let miss = sign_claims(&claims(now), &signing, Some("unknown"), "EdDSA").expect("miss");
        assert_eq!(
            auth.authenticate(&bearer(&miss)).await.unwrap_err(),
            AuthError::Unauthorized
        );
        assert_eq!(state.calls.load(Ordering::SeqCst), 2);
        assert!(
            auth.require_read(&bearer(&token), "acme", "app")
                .await
                .is_ok()
        );
        assert_eq!(state.calls.load(Ordering::SeqCst), 2);
        task.abort();
    }

    #[tokio::test]
    async fn forwarded_permissions_keep_the_existing_contract() {
        let mut config = gitcask_config::Config::default();
        config.server.auth_mode = AuthMode::Forwarded;
        let auth = Authenticator::new(&config).await.expect("auth");
        let mut headers = HeaderMap::new();
        headers.insert(PRINCIPAL_HEADER, "alice".parse().expect("header"));
        assert!(auth.require_read(&headers, "acme", "app").await.is_ok());
        assert_eq!(
            auth.require_write(&headers, "acme", "app")
                .await
                .unwrap_err(),
            AuthError::Forbidden
        );
        headers.insert(WRITE_HEADER, "1".parse().expect("header"));
        assert!(auth.require_write(&headers, "acme", "app").await.is_ok());
    }
}
