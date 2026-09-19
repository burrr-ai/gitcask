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
type Pending = watch::Receiver<Option<SharedAnswer>>;

pub(super) struct IntrospectClient {
    config: IntrospectConfig,
    secret: String,
    client: reqwest::Client,
    state: Box<Mutex<State>>,
}

#[derive(Default)]
struct State {
    cache: HashMap<Key, SharedAnswer>,
    // FIFO gives constant-time eviction and a strict cap without another
    // dependency. Expired entries may stay until replaced, but never authorize.
    order: VecDeque<Key>,
    flights: HashMap<Key, Pending>,
}

#[derive(Clone)]
struct SharedAnswer {
    answer: Answer,
    // None is explicitly uncacheable (zero TTL), but still joins this flight.
    expires: Option<Instant>,
}

impl SharedAnswer {
    fn new(answer: Answer, ttl: Duration) -> Self {
        let now = Instant::now();
        Self {
            answer,
            // An unrepresentable deadline expires immediately, never grants
            // an effectively unlimited lifetime by falling into the zero case.
            expires: (!ttl.is_zero()).then(|| now.checked_add(ttl).unwrap_or(now)),
        }
    }

    fn into_current(self) -> Answer {
        if self
            .expires
            .is_some_and(|expires| Instant::now() >= expires)
        {
            Err(AuthError::IntrospectionUnavailable)
        } else {
            self.answer
        }
    }
}

impl State {
    fn insert(&mut self, key: Key, shared: SharedAnswer) {
        if shared
            .expires
            .is_none_or(|expires| Instant::now() >= expires)
        {
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
        self.cache.insert(key, shared);
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
    sender: watch::Sender<Option<SharedAnswer>>,
}

impl Flight<'_> {
    fn complete(self, answer: Answer, ttl: Duration) -> Answer {
        let shared = SharedAnswer::new(answer, ttl);
        self.owner.state.lock().insert(self.key, shared.clone());
        self.sender.send_replace(Some(shared.clone()));
        shared.into_current()
    }
}

impl Drop for Flight<'_> {
    fn drop(&mut self) {
        let mut state = self.owner.state.lock();
        if self.sender.borrow().is_none() {
            self.sender.send_replace(Some(SharedAnswer::new(
                Err(AuthError::IntrospectionUnavailable),
                Duration::ZERO,
            )));
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
        if let Some(cached) = state
            .cache
            .get(&key)
            .filter(|c| c.expires.is_some_and(|expires| Instant::now() < expires))
        {
            count("hit");
            return Lookup::Ready(cached.clone().into_current());
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
                .map_or(
                    Err(AuthError::IntrospectionUnavailable),
                    SharedAnswer::into_current,
                ),
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
struct IntrospectionResponse {
    active: bool,
    #[serde(default)]
    principal: ResponseField<String>,
    #[serde(default)]
    scopes: ResponseField<Vec<String>>,
    #[serde(default)]
    ttl: ResponseField<u64>,
}

// Decode known fields directly, retaining serde's duplicate-field rejection.
// Invalid field values are tolerated only so active:false can ignore them;
// for active:true they reject the token. Missing and explicit null differ.
#[derive(Deserialize, Default)]
#[serde(untagged)]
enum ResponseField<T> {
    Valid(T),
    Invalid(serde::de::IgnoredAny),
    #[default]
    #[serde(skip)]
    Missing,
}

fn parse_response(
    body: &[u8],
    cache_ttl: Duration,
) -> Result<(Option<Principal>, Duration), &'static str> {
    let response: IntrospectionResponse = serde_json::from_slice(body)
        .map_err(|_| "malformed or ambiguous introspection response")?;
    if !response.active {
        return Ok((None, Duration::ZERO)); // All other field values are ignored.
    }
    let ttl = match response.ttl {
        ResponseField::Missing => Some(cache_ttl),
        ResponseField::Valid(ttl) => Some(Duration::from_secs(ttl).min(cache_ttl)),
        ResponseField::Invalid(_) => None,
    };
    if let (ResponseField::Valid(name), ResponseField::Valid(scopes), Some(ttl)) =
        (response.principal, response.scopes, ttl)
        && !name.is_empty()
        && let Ok(scopes) = parse_scopes(&scopes)
    {
        return Ok((
            Some(Principal {
                name,
                write: false,
                admin: false,
                anonymous: false,
                scopes: Some(scopes),
            }),
            ttl,
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
        started: tokio::sync::Notify,
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
        fake.started.notify_one();
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
                started: tokio::sync::Notify::new(),
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

    fn grant() -> Principal {
        parse_response(active().as_bytes(), Duration::from_secs(30))
            .unwrap()
            .0
            .unwrap()
    }

    struct HttpServer(tokio::task::JoinHandle<()>);

    impl Drop for HttpServer {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    async fn router_client(app: Router, timeout: Duration) -> (IntrospectClient, HttpServer) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = IntrospectConfig {
            url: format!("http://{}/i", listener.local_addr().unwrap()),
            timeout,
            ..IntrospectConfig::default()
        };
        let server = HttpServer(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        (
            IntrospectClient::with_secret(&config, "test-secret".into()).unwrap(),
            server,
        )
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
            json!({"active": false, "principal": {}, "scopes": null, "ttl": null}),
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
        let extras = br#"{"active":true,"principal":"p","scopes":[],"extra":false,"extra":true}"#;
        assert!(parse_response(extras, cap).unwrap().0.is_some());
    }

    #[tokio::test]
    async fn duplicate_security_fields_are_uncached_service_errors() {
        // Raw bytes matter: json! would erase the ambiguity before we test it.
        for body in [
            r#"{"active":false,"active":true,"principal":"p","scopes":["acme/shop:admin"]}"#,
            r#"{"active":true,"principal":"p","principal":"admin","scopes":[]}"#,
            r#"{"active":true,"principal":null,"principal":"p","scopes":[]}"#,
            r#"{"active":true,"principal":"p","scopes":[],"scopes":["acme/shop:admin"]}"#,
            r#"{"active":true,"principal":"p","scopes":[],"ttl":0,"ttl":600}"#,
            r#"{"active":true,"principal":"p","scopes":[],"ttl":null,"ttl":600}"#,
            r#"{"active":false,"principal":"p","principal":"other"}"#,
            r#"{"active":false,"\u0061ctive":true,"principal":"p","scopes":[]}"#,
        ] {
            assert!(
                parse_response(body.as_bytes(), Duration::from_mins(10)).is_err(),
                "{body}"
            );
            let service = Service::new(
                StatusCode::OK,
                body.into(),
                Duration::ZERO,
                Duration::from_secs(30),
            )
            .await;
            for _ in 0..2 {
                assert_eq!(
                    service.client.verify("opaque").await.unwrap_err(),
                    AuthError::IntrospectionUnavailable
                );
            }
            assert_eq!(service.calls(), 2);
            assert!(service.client.state.lock().cache.is_empty());
            assert!(service.client.state.lock().flights.is_empty());
        }
    }

    #[test]
    fn present_non_integer_ttls_are_invalid_including_null() {
        for ttl in [
            "null",
            "-1",
            "1.5",
            "1.0",
            "true",
            r#""30""#,
            "18446744073709551616",
            "{}",
            "[]",
        ] {
            let body = format!(r#"{{"active":true,"principal":"p","scopes":[],"ttl":{ttl}}}"#);
            assert!(
                parse_response(body.as_bytes(), Duration::from_secs(30))
                    .unwrap()
                    .0
                    .is_none(),
                "{body}"
            );
        }
    }

    #[tokio::test]
    async fn positive_and_negative_answers_expire() {
        for body in [
            active(),
            r#"{"active":false}"#.into(),
            r#"{"active":true,"principal":"p","scopes":["bad"]}"#.into(),
            r#"{"active":true,"principal":"p","scopes":[],"ttl":null}"#.into(),
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
        for (status, ttl) in [
            (StatusCode::OK, Duration::from_secs(30)),
            (StatusCode::OK, Duration::ZERO),
            (StatusCode::SERVICE_UNAVAILABLE, Duration::ZERO),
        ] {
            let service = Service::new(status, active(), Duration::from_millis(100), ttl).await;
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
            if ttl.is_zero() {
                assert!(service.client.state.lock().cache.is_empty());
            }
        }
    }

    #[tokio::test]
    async fn delayed_follower_cannot_use_expired_grant_after_revocation() {
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
        let Lookup::Wait(pending) = service.client.lookup(key) else {
            panic!("expected shared flight");
        };
        let mut follower = Box::pin(service.client.verify("opaque"));
        assert!(futures::poll!(follower.as_mut()).is_pending());
        let _ = leader.complete(Ok(grant()), Duration::from_millis(10));
        // The cached answer and watch result must carry one identical deadline.
        assert_eq!(
            service.client.state.lock().cache.get(&key).unwrap().expires,
            pending.borrow().as_ref().unwrap().expires
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
        let Lookup::Fetch(refresh) = service.client.lookup(key) else {
            panic!("expected expired grant");
        };
        assert_eq!(
            refresh
                .complete(Err(AuthError::Unauthorized), Duration::from_secs(30))
                .unwrap_err(),
            AuthError::Unauthorized
        );
        assert_eq!(
            service.client.verify("opaque").await.unwrap_err(),
            AuthError::Unauthorized
        );
        // Resume the old waiter only after a newer revocation has been cached.
        assert_eq!(
            follower.await.unwrap_err(),
            AuthError::IntrospectionUnavailable
        );
        assert_eq!(service.calls(), 0);
    }

    #[tokio::test]
    async fn redirect_never_reaches_its_location_with_a_request_or_credentials() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let target = received.clone();
        let app = Router::new()
            .route(
                "/i",
                axum::routing::post(|| async {
                    (StatusCode::TEMPORARY_REDIRECT, [("location", "/target")])
                }),
            )
            .route(
                "/target",
                axum::routing::any(move |headers: HeaderMap, body: axum::body::Bytes| {
                    let target = target.clone();
                    async move {
                        target.lock().push((headers, body));
                        Json(json!({"active":true,"principal":"p","scopes":[]}))
                    }
                }),
            );
        let (client, _server) = router_client(app, Duration::from_millis(500)).await;
        for _ in 0..2 {
            assert_eq!(
                client.verify("client-credential").await.unwrap_err(),
                AuthError::IntrospectionUnavailable
            );
        }
        // Same-origin 307 would forward both the service header and token body
        // if Policy::none were removed; the target must receive neither.
        assert!(received.lock().is_empty());
        assert!(client.state.lock().cache.is_empty());
        assert!(client.state.lock().flights.is_empty());
    }

    #[tokio::test]
    async fn body_stall_times_out_without_caching_or_leaking_a_flight() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let app = Router::new().route(
            "/i",
            axum::routing::post(move || {
                counter.fetch_add(1, Ordering::SeqCst);
                async {
                    let first = futures::stream::once(async {
                        Ok::<_, std::io::Error>(bytes::Bytes::from_static(br#"{"active":true,"#))
                    });
                    // Headers and a body chunk are sent now; the rest never arrives.
                    let body =
                        axum::body::Body::from_stream(first.chain(futures::stream::pending()));
                    ([("content-type", "application/json")], body)
                }
            }),
        );
        let (client, _server) = router_client(app, Duration::from_millis(100)).await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), client.fetch("opaque"))
                .await
                .unwrap()
                .unwrap_err(),
            "response read failed or timed out"
        );
        for _ in 0..2 {
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(2), client.verify("opaque"))
                    .await
                    .unwrap()
                    .unwrap_err(),
                AuthError::IntrospectionUnavailable
            );
            assert!(client.state.lock().cache.is_empty());
            assert!(client.state.lock().flights.is_empty());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn full_flight_table_admits_cached_hits_and_existing_followers_only() {
        let service = Service::new(
            StatusCode::OK,
            active(),
            Duration::ZERO,
            Duration::from_secs(30),
        )
        .await;
        let cached = Sha256::digest(b"cached").into();
        service.client.state.lock().insert(
            cached,
            SharedAnswer::new(Ok(grant()), Duration::from_secs(30)),
        );
        let mut leaders = Vec::new();
        for index in 0..MAX_ENTRIES {
            let key = Sha256::digest(index.to_string().as_bytes()).into();
            let Lookup::Fetch(leader) = service.client.lookup(key) else {
                panic!("expected available slot");
            };
            leaders.push(leader);
        }
        assert_eq!(service.client.state.lock().flights.len(), MAX_ENTRIES);
        let error = service
            .client
            .verify("distinct-over-capacity")
            .await
            .unwrap_err();
        let response = crate::error::ApiError::from(error).into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers().get("retry-after").unwrap(), "5");
        service.client.verify("cached").await.unwrap();
        let existing = (MAX_ENTRIES - 1).to_string();
        let mut follower = Box::pin(service.client.verify(&existing));
        assert!(futures::poll!(follower.as_mut()).is_pending());
        leaders
            .pop()
            .unwrap()
            .complete(Ok(grant()), Duration::from_secs(30))
            .unwrap();
        follower.await.unwrap();
        assert_eq!(service.client.state.lock().flights.len(), MAX_ENTRIES - 1);
        drop(leaders);
        assert!(service.client.state.lock().flights.is_empty());
        assert_eq!(service.calls(), 0);
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

    #[tokio::test]
    async fn aborting_an_http_leader_and_dropping_a_follower_releases_the_flight() {
        let service = Service::new(
            StatusCode::OK,
            active(),
            Duration::from_secs(2),
            Duration::from_secs(30),
        )
        .await;
        let client = service.client.clone();
        let leader = tokio::spawn(async move { client.verify("opaque").await });
        tokio::time::timeout(Duration::from_secs(2), service.fake.started.notified())
            .await
            .unwrap();
        let mut cancelled = Box::pin(service.client.verify("opaque"));
        assert!(futures::poll!(cancelled.as_mut()).is_pending());
        drop(cancelled);
        assert_eq!(service.client.state.lock().flights.len(), 1);
        let mut waiting = Box::pin(service.client.verify("opaque"));
        assert!(futures::poll!(waiting.as_mut()).is_pending());
        leader.abort();
        assert!(
            tokio::time::timeout(Duration::from_secs(2), leader)
                .await
                .unwrap()
                .unwrap_err()
                .is_cancelled()
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), waiting)
                .await
                .unwrap()
                .unwrap_err(),
            AuthError::IntrospectionUnavailable
        );
        assert!(service.client.state.lock().flights.is_empty());
        assert!(service.client.state.lock().cache.is_empty());
        assert_eq!(service.calls(), 1);
        let Lookup::Fetch(next) = service.client.lookup(Sha256::digest(b"opaque").into()) else {
            panic!("expected released slot");
        };
        drop(next);
    }

    #[tokio::test]
    async fn unwinding_a_leader_releases_the_follower_and_slot() {
        let service = Service::new(
            StatusCode::OK,
            active(),
            Duration::ZERO,
            Duration::from_secs(30),
        )
        .await;
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let client = service.client.clone();
        let leader = tokio::spawn(async move {
            let Lookup::Fetch(_flight) = client.lookup(Sha256::digest(b"opaque").into()) else {
                panic!("expected leader");
            };
            started.send(()).unwrap();
            released.await.unwrap();
            panic!("test leader unwind");
        });
        tokio::time::timeout(Duration::from_secs(2), ready)
            .await
            .unwrap()
            .unwrap();
        let mut follower = Box::pin(service.client.verify("opaque"));
        assert!(futures::poll!(follower.as_mut()).is_pending());
        release.send(()).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(2), leader)
                .await
                .unwrap()
                .unwrap_err()
                .is_panic()
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), follower)
                .await
                .unwrap()
                .unwrap_err(),
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
            state.insert(
                key,
                SharedAnswer::new(Err(AuthError::Unauthorized), Duration::from_secs(30)),
            );
        }
        assert_eq!(state.cache.len(), MAX_ENTRIES);
        assert_eq!(state.order.len(), MAX_ENTRIES);
        let oldest: Key = Sha256::digest(b"0").into();
        assert!(!state.cache.contains_key(&oldest));
        let latest: Key = Sha256::digest(MAX_ENTRIES.to_string().as_bytes()).into();
        for _ in 0..10 {
            state.insert(
                latest,
                SharedAnswer::new(Err(AuthError::Unauthorized), Duration::from_secs(30)),
            );
        }
        assert_eq!(state.order.len(), MAX_ENTRIES);
    }
}
