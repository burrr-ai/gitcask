//! Issuer-owned opaque tokens. Only bounded, disposable answers and their
//! SHA-256 keys survive a request; credentials never enter either map.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, ensure};
use futures::StreamExt as _;
use gitcask_config::IntrospectConfig;
use parking_lot::Mutex;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use tokio::sync::watch;

use super::{AuthError, Principal, parse_scopes};

const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_ENTRIES: usize = 10_000;
type Key = [u8; 32];
type Answer = Result<Principal, AuthError>;
type Pending = watch::Receiver<Option<Answer>>;

pub(super) struct IntrospectClient {
    config: IntrospectConfig,
    secret: String,
    client: reqwest::Client,
    state: Box<Mutex<State>>,
}

#[derive(Default)]
struct State {
    cache: HashMap<Key, Cached>,
    // FIFO gives constant-time eviction and a strict cap without another
    // dependency. Expired entries may stay until replaced, but never authorize.
    order: VecDeque<Key>,
    flights: HashMap<Key, Pending>,
}

struct Cached {
    answer: Answer,
    expires: Instant,
}

impl State {
    fn insert(&mut self, key: Key, answer: Answer, ttl: Duration) {
        let Some(expires) = Instant::now().checked_add(ttl) else {
            return;
        };
        if ttl.is_zero() {
            return;
        }
        if !self.cache.contains_key(&key) {
            if self.cache.len() == MAX_ENTRIES
                && let Some(oldest) = self.order.pop_front()
            {
                self.cache.remove(&oldest);
            }
            self.order.push_back(key);
        }
        self.cache.insert(key, Cached { answer, expires });
    }
}

enum Lookup<'a> {
    Ready(Answer),
    Wait(Pending),
    Fetch(Flight<'a>),
}

// The leader borrows the request's credential; no detached task retains it.
// Dropping/cancelling the leader must also release every follower and map slot.
struct Flight<'a> {
    owner: &'a IntrospectClient,
    key: Key,
    sender: watch::Sender<Option<Answer>>,
}

impl Flight<'_> {
    fn complete(self, answer: Answer, ttl: Duration) -> Answer {
        self.owner
            .state
            .lock()
            .insert(self.key, answer.clone(), ttl);
        self.sender.send_replace(Some(answer.clone()));
        answer
    }
}

impl Drop for Flight<'_> {
    fn drop(&mut self) {
        let mut state = self.owner.state.lock();
        if self.sender.borrow().is_none() {
            self.sender
                .send_replace(Some(Err(AuthError::IntrospectionUnavailable)));
        }
        state.flights.remove(&self.key);
    }
}

impl IntrospectClient {
    pub(super) fn new(config: &IntrospectConfig) -> Result<Self> {
        // VarError::NotUnicode contains the value: discard it, not just its
        // outer context, so even a malformed secret cannot enter startup logs.
        let secret = std::env::var(&config.secret_env)
            .map_err(|_| anyhow::anyhow!("cannot read auth.introspect.secret_env at startup"))?;
        ensure!(!secret.is_empty(), "introspection bearer secret is empty");
        Self::with_secret(config, secret)
    }

    fn with_secret(config: &IntrospectConfig, secret: String) -> Result<Self> {
        Ok(Self {
            config: config.clone(),
            secret,
            client: reqwest::Client::builder()
                .connect_timeout(config.timeout)
                // Every non-200 is a service error, including redirects. Never
                // forward the token body or service secret to another endpoint.
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .context("building introspection client")?,
            state: Box::default(),
        })
    }

    fn lookup(&self, key: Key) -> Lookup<'_> {
        let mut state = self.state.lock();
        if let Some(cached) = state.cache.get(&key).filter(|c| Instant::now() < c.expires) {
            count("hit");
            return Lookup::Ready(cached.answer.clone());
        }
        count("miss");
        if let Some(pending) = state.flights.get(&key) {
            return Lookup::Wait(pending.clone());
        }
        // The in-flight table is bounded too, even under a unique-token flood.
        if state.flights.len() == MAX_ENTRIES {
            return Lookup::Ready(Err(AuthError::IntrospectionUnavailable));
        }
        let (sender, receiver) = watch::channel(None);
        state.flights.insert(key, receiver);
        Lookup::Fetch(Flight {
            owner: self,
            key,
            sender,
        })
    }

    pub(super) async fn verify(&self, token: &str) -> Answer {
        match self.lookup(Sha256::digest(token.as_bytes()).into()) {
            Lookup::Ready(answer) => answer,
            Lookup::Wait(mut pending) => pending
                .wait_for(Option::is_some)
                .await
                .ok()
                .and_then(|answer| answer.clone())
                .unwrap_or(Err(AuthError::IntrospectionUnavailable)),
            Lookup::Fetch(flight) => {
                let started = Instant::now();
                let response = self.fetch(token).await;
                metrics::histogram!("gitcask_auth_introspect_seconds")
                    .record(started.elapsed().as_secs_f64());
                let (answer, ttl) = match response {
                    Ok((Some(principal), ttl)) => {
                        count("active");
                        (Ok(principal), ttl)
                    }
                    Ok((None, _)) => {
                        count("inactive");
                        (Err(AuthError::Unauthorized), self.config.negative_cache_ttl)
                    }
                    Err(cause) => {
                        count("error");
                        // Causes are fixed strings: never log request/response
                        // bodies, credentials, or reqwest errors containing URLs.
                        tracing::warn!(cause, "introspection service unavailable");
                        (Err(AuthError::IntrospectionUnavailable), Duration::ZERO)
                    }
                };
                flight.complete(answer, ttl)
            }
        }
    }

    async fn fetch(&self, token: &str) -> Result<(Option<Principal>, Duration), &'static str> {
        let response = self
            .client
            .post(&self.config.url)
            .bearer_auth(&self.secret)
            .timeout(self.config.timeout)
            .json(&serde_json::json!({ "token": token }))
            .send()
            .await
            .map_err(|_| "connection or request timeout")?;
        match response.status() {
            reqwest::StatusCode::OK => {}
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
                return Err("endpoint rejected gitcask's bearer secret (401/403)");
            }
            _ => return Err("endpoint returned non-200 status"),
        }
        if response
            .content_length()
            .is_some_and(|size| size > MAX_BODY_BYTES as u64)
        {
            return Err("response exceeds 64 KiB");
        }
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| "response read failed or timed out")?;
            if body.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
                return Err("response exceeds 64 KiB");
            }
            body.extend_from_slice(&chunk);
        }
        parse_response(&body, self.config.cache_ttl)
    }
}

fn count(outcome: &'static str) {
    metrics::counter!("gitcask_auth_introspect_total", "outcome" => outcome).increment(1);
}

#[derive(Deserialize)]
struct ActiveResponse {
    principal: String,
    scopes: Vec<String>,
    ttl: Option<u64>,
}

fn parse_response(
    body: &[u8],
    cache_ttl: Duration,
) -> Result<(Option<Principal>, Duration), &'static str> {
    let value: serde_json::Value = serde_json::from_slice(body).map_err(|_| "malformed JSON")?;
    match value.get("active").and_then(serde_json::Value::as_bool) {
        Some(false) => return Ok((None, Duration::ZERO)), // All other fields are ignored.
        Some(true) => {}
        None => return Err("missing or invalid active flag"),
    }
    let answer = serde_json::from_value::<ActiveResponse>(value)
        .ok()
        .filter(|answer| !answer.principal.is_empty());
    if let Some(answer) = answer
        && let Ok(scopes) = parse_scopes(&answer.scopes)
    {
        return Ok((
            Some(Principal {
                name: answer.principal,
                write: false,
                admin: false,
                anonymous: false,
                scopes: Some(scopes),
            }),
            answer
                .ttl
                .map_or(cache_ttl, |ttl| Duration::from_secs(ttl).min(cache_ttl)),
        ));
    }
    tracing::debug!("invalid introspection principal, scopes or ttl");
    Ok((None, Duration::ZERO))
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{
        Json, Router,
        extract::State as AxumState,
        http::{HeaderMap, StatusCode},
        response::IntoResponse as _,
    };
    use serde_json::json;

    use super::*;

    struct Fake {
        calls: AtomicUsize,
        status: StatusCode,
        body: String,
        delay: Duration,
    }

    async fn respond(
        AxumState(fake): AxumState<Arc<Fake>>,
        headers: HeaderMap,
        Json(body): Json<serde_json::Value>,
    ) -> axum::response::Response {
        assert_eq!(headers.get("authorization").unwrap(), "Bearer test-secret");
        assert_eq!(headers.get("content-type").unwrap(), "application/json");
        assert!(body.get("token").unwrap().is_string());
        fake.calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(fake.delay).await;
        // Stream without Content-Length so the cap is also tested while reading.
        let body = axum::body::Body::from_stream(futures::stream::iter(
            fake.body
                .as_bytes()
                .chunks(8192)
                .map(|chunk| Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(chunk)))
                .collect::<Vec<_>>(),
        ));
        (fake.status, [("content-type", "application/json")], body).into_response()
    }

    struct Service {
        fake: Arc<Fake>,
        client: Arc<IntrospectClient>,
        server: tokio::task::JoinHandle<()>,
    }

    impl Service {
        async fn new(status: StatusCode, body: String, delay: Duration, ttl: Duration) -> Self {
            let fake = Arc::new(Fake {
                calls: AtomicUsize::new(0),
                status,
                body,
                delay,
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let config = IntrospectConfig {
                url: format!("http://{}/i", listener.local_addr().unwrap()),
                cache_ttl: ttl,
                negative_cache_ttl: ttl,
                timeout: Duration::from_millis(500),
                ..IntrospectConfig::default()
            };
            let app = Router::new()
                .route("/i", axum::routing::post(respond))
                .with_state(fake.clone());
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            Self {
                fake,
                client: Arc::new(
                    IntrospectClient::with_secret(&config, "test-secret".into()).unwrap(),
                ),
                server,
            }
        }

        fn calls(&self) -> usize {
            self.fake.calls.load(Ordering::SeqCst)
        }
    }

    impl Drop for Service {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    fn active() -> String {
        json!({"active": true, "principal": "user:42", "scopes": ["acme/shop:write", "acme/*:read"]}).to_string()
    }

    #[test]
    fn response_shapes_and_ttl_caps() {
        let cap = Duration::from_secs(30);
        let (principal, ttl) = parse_response(active().as_bytes(), cap).unwrap();
        let principal = principal.unwrap();
        assert_eq!(principal.name, "user:42");
        assert_eq!(ttl, cap);
        assert_eq!(
            principal.permission_for(&super::super::Repository::new("acme", "shop").unwrap()),
            Some(super::super::Permission::Write)
        );
        for (ttl, expected) in [(0, 0), (2, 2), (300, 30), (u64::MAX, 30)] {
            let body = json!({"active": true, "principal": "p", "scopes": [], "ttl": ttl});
            let (principal, lifetime) = parse_response(body.to_string().as_bytes(), cap).unwrap();
            assert!(principal.unwrap().scopes.unwrap().is_empty());
            assert_eq!(lifetime, Duration::from_secs(expected));
        }
        for body in [
            json!({"active": false}),
            json!({"active": false, "principal": 5, "scopes": "ignored", "ttl": -1}),
            json!({"active": true}),
            json!({"active": true, "principal": "", "scopes": []}),
            json!({"active": true, "principal": 5, "scopes": []}),
            json!({"active": true, "principal": "p", "scopes": "acme/shop:read"}),
            json!({"active": true, "principal": "p", "scopes": ["acme/shop:read", "*/shop:admin"]}),
            json!({"active": true, "principal": "p", "scopes": ["acme/shop:owner"]}),
            json!({"active": true, "principal": "p", "scopes": [], "ttl": -1}),
        ] {
            assert!(
                parse_response(body.to_string().as_bytes(), cap)
                    .unwrap()
                    .0
                    .is_none(),
                "{body}"
            );
        }
        for body in ["{", "{}", "null", "[]", r#"{"active":"true"}"#] {
            assert!(parse_response(body.as_bytes(), cap).is_err(), "{body}");
        }
    }

    #[tokio::test]
    async fn positive_and_negative_answers_expire() {
        for body in [
            active(),
            r#"{"active":false}"#.into(),
            r#"{"active":true,"principal":"p","scopes":["bad"]}"#.into(),
        ] {
            let service = Service::new(
                StatusCode::OK,
                body,
                Duration::ZERO,
                Duration::from_millis(200),
            )
            .await;
            let first = service.client.verify("opaque").await;
            assert_ne!(
                first.as_ref().err(),
                Some(&AuthError::IntrospectionUnavailable)
            );
            assert_eq!(first.is_ok(), service.client.verify("opaque").await.is_ok());
            assert_eq!(service.calls(), 1);
            tokio::time::sleep(Duration::from_millis(250)).await;
            assert_eq!(first.is_ok(), service.client.verify("opaque").await.is_ok());
            assert_eq!(service.calls(), 2);
        }
    }

    #[tokio::test]
    async fn zero_ttl_disables_positive_cache() {
        let service = Service::new(
            StatusCode::OK,
            json!({"active":true,"principal":"p","scopes":[],"ttl":0}).to_string(),
            Duration::ZERO,
            Duration::from_secs(30),
        )
        .await;
        service.client.verify("opaque").await.unwrap();
        service.client.verify("opaque").await.unwrap();
        assert_eq!(service.calls(), 2);
        assert!(service.client.state.lock().cache.is_empty());
    }

    #[tokio::test]
    async fn service_failures_are_never_cached() {
        for (status, body, delay) in [
            (StatusCode::INTERNAL_SERVER_ERROR, active(), Duration::ZERO),
            (StatusCode::UNAUTHORIZED, active(), Duration::ZERO),
            (StatusCode::FORBIDDEN, active(), Duration::ZERO),
            (StatusCode::FOUND, active(), Duration::ZERO),
            (StatusCode::NO_CONTENT, String::new(), Duration::ZERO),
            (StatusCode::OK, "{".into(), Duration::ZERO),
            (
                StatusCode::OK,
                active() + &" ".repeat(MAX_BODY_BYTES),
                Duration::ZERO,
            ),
            (StatusCode::OK, active(), Duration::from_secs(2)),
        ] {
            let service = Service::new(status, body, delay, Duration::from_secs(30)).await;
            for _ in 0..2 {
                assert_eq!(
                    service.client.verify("opaque").await.unwrap_err(),
                    AuthError::IntrospectionUnavailable
                );
            }
            assert_eq!(service.calls(), 2);
            assert!(service.client.state.lock().cache.is_empty());
        }
    }

    #[tokio::test]
    async fn concurrent_misses_share_success_and_failure() {
        for status in [StatusCode::OK, StatusCode::SERVICE_UNAVAILABLE] {
            let service = Service::new(
                status,
                active(),
                Duration::from_millis(100),
                Duration::from_secs(30),
            )
            .await;
            let mut tasks = Vec::new();
            for _ in 0..32 {
                let client = service.client.clone();
                tasks.push(tokio::spawn(
                    async move { client.verify("same-token").await },
                ));
            }
            for task in tasks {
                assert_eq!(task.await.unwrap().is_ok(), status == StatusCode::OK);
            }
            assert_eq!(service.calls(), 1);
            assert!(service.client.state.lock().flights.is_empty());
        }
    }

    #[tokio::test]
    async fn cancelled_leader_releases_waiters_and_slot() {
        let service = Service::new(
            StatusCode::OK,
            active(),
            Duration::ZERO,
            Duration::from_secs(30),
        )
        .await;
        let key = Sha256::digest(b"opaque").into();
        let Lookup::Fetch(leader) = service.client.lookup(key) else {
            panic!("expected leader");
        };
        let client = service.client.clone();
        let follower = tokio::spawn(async move { client.verify("opaque").await });
        tokio::task::yield_now().await;
        drop(leader);
        assert_eq!(
            follower.await.unwrap().unwrap_err(),
            AuthError::IntrospectionUnavailable
        );
        assert!(service.client.state.lock().flights.is_empty());
        service.client.verify("opaque").await.unwrap();
        assert_eq!(service.calls(), 1);
    }

    #[test]
    fn cache_is_strictly_bounded_and_keyed_by_digests() {
        let mut state = State::default();
        for index in 0..=MAX_ENTRIES {
            let key = Sha256::digest(index.to_string().as_bytes()).into();
            state.insert(key, Err(AuthError::Unauthorized), Duration::from_secs(30));
        }
        assert_eq!(state.cache.len(), MAX_ENTRIES);
        assert_eq!(state.order.len(), MAX_ENTRIES);
        let oldest: Key = Sha256::digest(b"0").into();
        assert!(!state.cache.contains_key(&oldest));
        let latest: Key = Sha256::digest(MAX_ENTRIES.to_string().as_bytes()).into();
        for _ in 0..10 {
            state.insert(
                latest,
                Err(AuthError::Unauthorized),
                Duration::from_secs(30),
            );
        }
        assert_eq!(state.order.len(), MAX_ENTRIES);
    }
}
