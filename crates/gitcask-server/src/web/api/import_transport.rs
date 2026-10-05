//! External Git can contact only this scoped loopback relay. The relay owns
//! HTTPS, public-address validation, DNS pinning, byte bounds and redirects.
use crate::error::ApiError;
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::State,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures::TryStreamExt;
use std::{
    net::{IpAddr, SocketAddr},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU16, AtomicU64, Ordering},
    },
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(super) fn validate_url(source: &str) -> Result<reqwest::Url, ApiError> {
    let url = reqwest::Url::parse(source)
        .map_err(|_| ApiError::BadRequest("invalid source URL".into()))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.port_or_known_default() != Some(443)
        || url.path() == "/"
        || source.len() > 2048
        || source.bytes().any(|b| b.is_ascii_control())
    {
        return Err(ApiError::BadRequest(
            "source must be credential-free HTTPS on port 443, without query or fragment".into(),
        ));
    }
    Ok(url)
}

pub(super) fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, d] = ip.octets();
            !(a == 0
                || a == 10
                || a == 127
                || a >= 224
                || (a == 100 && (64..128).contains(&b))
                || (a == 168 && b == 63 && c == 129 && d == 16) // Azure platform virtual IP
                || (a == 169 && b == 254)
                || (a == 172 && (16..32).contains(&b))
                || (a == 192 && (b == 168 || (b == 88 && c == 99) || (b == 0 && (c == 0 || c == 2))))
                || (a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
                || (a == 203 && b == 0 && c == 113))
        }
        IpAddr::V6(ip) => {
            let [first, second, ..] = ip.segments();
            // Only native global unicast; reject transition/tunnel, special
            // purpose and documentation ranges as well as mapped IPv4.
            (first & 0xe000) == 0x2000
                && first != 0x2002
                && !(first == 0x2001 && (second < 0x200 || second == 0xdb8))
                && !(first == 0x3fff && second < 0x1000)
        }
    }
}

#[derive(Clone)]
struct RelayState {
    client: reqwest::Client,
    base: reqwest::Url,
    bytes: Arc<AtomicU64>,
    max_bytes: u64,
    failure: Arc<AtomicU16>,
}

pub(super) struct Relay {
    pub url: String,
    failure: Arc<AtomicU16>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Relay {
    pub fn error(&self, fallback: ApiError) -> ApiError {
        match self.failure.load(Ordering::Relaxed) {
            413 => ApiError::PayloadTooLarge,
            503 => ApiError::ImportUnavailable("source transport unavailable".into()),
            422 => ApiError::UnprocessableEntity(
                "source must expose public smart HTTP without redirects".into(),
            ),
            _ => fallback,
        }
    }
    pub async fn start(source: &str, max_bytes: u64) -> Result<Self, ApiError> {
        let base = validate_url(source)?;
        let host = base
            .host_str()
            .ok_or_else(|| ApiError::BadRequest("missing URL host".into()))?
            .trim_matches(['[', ']']);
        let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host, 443))
            .await
            .map_err(|_| ApiError::ImportUnavailable("source DNS lookup failed".into()))?
            .collect();
        if addresses.is_empty()
            || addresses.len() > 32
            || addresses.iter().any(|a| !public_ip(a.ip()))
        {
            return Err(ApiError::BadRequest(
                "source DNS must contain only public IP addresses".into(),
            ));
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .resolve_to_addrs(host, &addresses)
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_mins(15))
            .build()
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        Self::with_client(base, client, max_bytes).await
    }
    async fn with_client(
        base: reqwest::Url,
        client: reqwest::Client,
        max_bytes: u64,
    ) -> Result<Self, ApiError> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        let address = listener
            .local_addr()
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        let failure = Arc::new(AtomicU16::new(0));
        let state = RelayState {
            client,
            base,
            bytes: Arc::new(AtomicU64::new(0)),
            max_bytes,
            failure: failure.clone(),
        };
        let router = Router::new()
            .route("/remote/info/refs", get(relay))
            .route("/remote/git-upload-pack", post(relay))
            .with_state(state);
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Ok(Self {
            url: format!("http://{address}/remote"),
            failure,
            task,
        })
    }
}

async fn relay(State(state): State<RelayState>, request: Request<Body>) -> Response {
    let result = async {
        let listing = request.method() == axum::http::Method::GET;
        if (listing && request.uri().query() != Some("service=git-upload-pack"))
            || (!listing && request.uri().query().is_some())
        {
            return Err(StatusCode::BAD_REQUEST);
        }
        let mut url = state.base.clone();
        url.set_path(&format!(
            "{}{suffix}",
            state.base.path().trim_end_matches('/'),
            suffix = if listing {
                "/info/refs"
            } else {
                "/git-upload-pack"
            }
        ));
        if listing {
            url.set_query(Some("service=git-upload-pack"));
        }
        let mut upstream = state.client.request(request.method().clone(), url);
        // No client Authorization, cookies, proxy, or arbitrary headers escape.
        if let Some(protocol) = request.headers().get("git-protocol") {
            upstream = upstream.header("git-protocol", protocol);
        }
        if !listing {
            upstream = upstream.header("content-type", "application/x-git-upload-pack-request");
            upstream = upstream.body(
                to_bytes(request.into_body(), 1024 * 1024)
                    .await
                    .map_err(|_| StatusCode::PAYLOAD_TOO_LARGE)?,
            );
        }
        let response = upstream.send().await.map_err(|_| {
            state.failure.store(503, Ordering::Relaxed);
            StatusCode::BAD_GATEWAY
        })?;
        if response.status().is_redirection() {
            state.failure.store(422, Ordering::Relaxed);
            return Err(StatusCode::UNPROCESSABLE_ENTITY);
        }
        if !response.status().is_success() {
            state.failure.store(
                if response.status().is_server_error() || response.status().as_u16() == 429 {
                    503
                } else {
                    422
                },
                Ordering::Relaxed,
            );
            return Err(StatusCode::UNPROCESSABLE_ENTITY);
        }
        let content_type = if listing {
            "application/x-git-upload-pack-advertisement"
        } else {
            "application/x-git-upload-pack-result"
        };
        if response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_none_or(|v| v.split(';').next() != Some(content_type))
        {
            state.failure.store(422, Ordering::Relaxed);
            return Err(StatusCode::UNPROCESSABLE_ENTITY);
        }
        let stream_failure = state.failure.clone();
        let stream = response
            .bytes_stream()
            .map_err(move |error| {
                stream_failure.store(503, Ordering::Relaxed);
                std::io::Error::other(error)
            })
            .and_then(move |chunk| {
                let used = state
                    .bytes
                    .fetch_add(chunk.len() as u64, Ordering::Relaxed)
                    .saturating_add(chunk.len() as u64);
                futures::future::ready(if used > state.max_bytes {
                    state.failure.store(413, Ordering::Relaxed);
                    Err(std::io::Error::other("import transfer byte limit exceeded"))
                } else {
                    Ok(chunk)
                })
            });
        Ok(([("content-type", content_type)], Body::from_stream(stream)).into_response())
    }
    .await;
    result.unwrap_or_else(IntoResponse::into_response)
}

pub(super) fn git(path: &Path) -> tokio::process::Command {
    let mut command = gitcask_git::isolated_command(path);
    command.stderr(std::process::Stdio::piped());
    command
}

pub(super) async fn output(
    command: tokio::process::Command,
    limit: usize,
) -> Result<Vec<u8>, ApiError> {
    output_input(command, limit, None).await
}
pub(super) async fn output_input(
    mut command: tokio::process::Command,
    limit: usize,
    input: Option<&[u8]>,
) -> Result<Vec<u8>, ApiError> {
    if input.is_some() {
        command.stdin(std::process::Stdio::piped());
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let mut group = gitcask_git::ImportProcess::new(child.id());
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| ApiError::Internal("git stdout".into()))?
        .take(limit as u64 + 1);
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| ApiError::Internal("git stderr".into()))?;
    let mut out = Vec::new();
    let feed = async {
        if let Some(input) = input {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| std::io::Error::other("Git stdin unavailable"))?;
            stdin.write_all(input).await?;
            stdin.shutdown().await?;
        }
        Ok::<_, std::io::Error>(())
    };
    let read_stdout = async {
        stdout.read_to_end(&mut out).await?;
        if out.len() > limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                "Git output limit exceeded",
            ));
        }
        Ok::<_, std::io::Error>(())
    };
    // Discard diagnostics with constant memory. Stopping a pipe reader at a
    // capture cap can deadlock a child writing further diagnostics or refs.
    let mut diagnostics = tokio::io::sink();
    tokio::try_join!(
        feed,
        read_stdout,
        tokio::io::copy(&mut stderr, &mut diagnostics)
    )
    .map_err(|e| {
        if e.kind() == std::io::ErrorKind::FileTooLarge {
            ApiError::PayloadTooLarge
        } else {
            ApiError::ImportUnavailable("Git acquisition I/O failed".into())
        }
    })?;
    if !child
        .wait()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .success()
    {
        // Don't disclose upstream content or scratch paths in public errors.
        return Err(ApiError::ImportUnavailable(
            "Git acquisition failed; retry the fixed snapshot".into(),
        ));
    }
    group.finished();
    Ok(out)
}

// Killing just `git fetch` leaves its HTTP helper/index-pack children alive.
// Every acquisition gets a fresh process group; cancellation kills that group
// on the bulk runtime, without unsafe syscalls or a blocking wait in Drop.

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_addresses_only() -> anyhow::Result<()> {
        for ip in [
            "127.0.0.1",
            "10.1.1.1",
            "169.254.169.254",
            "168.63.129.16",
            "192.88.99.1",
            "100.100.100.200",
            "192.168.1.1",
            "0.0.0.0",
            "198.18.0.1",
            "::1",
            "fe80::1",
            "fc00::1",
            "::ffff:8.8.8.8",
            "2002:0808:0808::1",
            "2001:db8::1",
        ] {
            assert!(!public_ip(ip.parse()?), "{ip}");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(public_ip(ip.parse()?), "{ip}");
        }
        for url in [
            "http://example.com/a",
            "https://user@example.com/a",
            "https://example.com:8443/a",
            "https://example.com/a?token=x",
            "https://example.com/a#x",
            "file:///tmp/repo",
        ] {
            assert!(validate_url(url).is_err());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tls_tests {
    use super::*;
    use anyhow::{Context, Result, ensure};
    use hyper_util::{
        rt::{TokioExecutor, TokioIo},
        server::conn::auto::Builder,
        service::TowerToHyperService,
    };
    use tokio::io::AsyncWriteExt;
    use tokio_rustls::{
        TlsAcceptor,
        rustls::{
            self,
            pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
        },
    };

    struct Origin {
        url: reqwest::Url,
        client: reqwest::Client,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for Origin {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    async fn origin(router: Router) -> Result<Origin> {
        let cert = include_bytes!("../../../tests/fixtures/import-tls/cert.der");
        let key = include_bytes!("../../../tests/fixtures/import-tls/key.der");
        let provider = rustls::crypto::ring::default_provider();
        let tls = rustls::ServerConfig::builder_with_provider(Arc::new(provider))
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(cert.to_vec())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.to_vec())),
            )?;
        let acceptor = TlsAcceptor::from(Arc::new(tls));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                let service = TowerToHyperService::new(router.clone());
                tokio::spawn(async move {
                    if let Ok(tls) = acceptor.accept(stream).await {
                        let _ = Builder::new(TokioExecutor::new())
                            .serve_connection_with_upgrades(TokioIo::new(tls), service)
                            .await;
                    }
                });
            }
        });
        let url = reqwest::Url::parse(&format!(
            "https://import-origin.test:{}/source",
            address.port()
        ))?;
        // This is test injection, never exposed through Config or env. Real
        // Relay::start validates public IPs and never trusts this test CA.
        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .add_root_certificate(reqwest::Certificate::from_der(cert)?)
            .resolve("import-origin.test", address)
            .build()?;
        Ok(Origin { url, client, task })
    }
    async fn smart(State(path): State<std::path::PathBuf>, request: Request<Body>) -> Response {
        let result = async {
            let listing = request.method() == axum::http::Method::GET;
            let mut command = git(&path);
            command.args(["upload-pack", "--stateless-rpc"]);
            if listing {
                command.arg("--advertise-refs");
            }
            command.arg(&path);
            if let Some(protocol) = request.headers().get("git-protocol") {
                command.env("GIT_PROTOCOL", protocol.to_str().unwrap_or_default());
            }
            let output = if listing {
                output(command, 1024 * 1024).await?
            } else {
                command.stdin(std::process::Stdio::piped());
                let mut child = command
                    .spawn()
                    .map_err(|e| ApiError::Internal(e.to_string()))?;
                let mut input = child
                    .stdin
                    .take()
                    .ok_or_else(|| ApiError::Internal("stdin".into()))?;
                let body = to_bytes(request.into_body(), 1024 * 1024)
                    .await
                    .map_err(|e| ApiError::Internal(e.to_string()))?;
                input
                    .write_all(&body)
                    .await
                    .map_err(|e| ApiError::Internal(e.to_string()))?;
                drop(input);
                child
                    .wait_with_output()
                    .await
                    .map_err(|e| ApiError::Internal(e.to_string()))?
                    .stdout
            };
            if listing {
                let mut bytes = Vec::new();
                gitcask_git::pkt::encode_data(&mut bytes, b"# service=git-upload-pack\n");
                gitcask_git::pkt::encode_flush(&mut bytes);
                bytes.extend(output);
                Ok((
                    [(
                        "content-type",
                        "application/x-git-upload-pack-advertisement",
                    )],
                    bytes,
                )
                    .into_response())
            } else {
                Ok((
                    [("content-type", "application/x-git-upload-pack-result")],
                    output,
                )
                    .into_response())
            }
        }
        .await;
        result.unwrap_or_else(|e: ApiError| e.into_response())
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn synthetic_tls_git_preserves_pinned_history_and_refuses_redirects() -> Result<()> {
        let source = tempfile::tempdir()?;
        for args in [
            vec!["init", "-q", "-b", "develop"],
            vec![
                "-c",
                "user.name=Synthetic",
                "-c",
                "user.email=synthetic@example.test",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "first",
            ],
            vec![
                "-c",
                "user.name=Synthetic",
                "-c",
                "user.email=synthetic@example.test",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "pinned second",
            ],
            vec![
                "-c",
                "user.name=Synthetic",
                "-c",
                "user.email=synthetic@example.test",
                "tag",
                "-a",
                "v1",
                "-m",
                "annotated",
            ],
        ] {
            let mut command = git(source.path());
            command.args(args);
            output(command, 1024)
                .await
                .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        }
        let source_origin = origin(
            Router::new()
                .route("/source/info/refs", get(smart))
                .route("/source/git-upload-pack", post(smart))
                .with_state(source.path().to_path_buf()),
        )
        .await?;
        let relay = Relay::with_client(
            source_origin.url.clone(),
            source_origin.client.clone(),
            1024 * 1024,
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let target = tempfile::tempdir()?;
        let mut command = git(target.path());
        command.args([
            "ls-remote",
            "--symref",
            &relay.url,
            "HEAD",
            "refs/heads/*",
            "refs/tags/*",
        ]);
        let listing = output(command, 65536)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let text = String::from_utf8(listing)?;
        ensure!(text.contains("ref: refs/heads/develop\tHEAD") && text.contains("refs/tags/v1"));
        let pinned = text
            .lines()
            .find_map(|l| l.strip_suffix("\trefs/heads/develop"))
            .context("advertised branch")?
            .to_string();
        let tag_oid = text
            .lines()
            .find_map(|l| l.strip_suffix("\trefs/tags/v1"))
            .context("advertised tag")?
            .to_string();
        // Move the source branch after pinning; fetch the original object IDs.
        let mut command = git(source.path());
        command.args([
            "-c",
            "user.name=Synthetic",
            "-c",
            "user.email=synthetic@example.test",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "second",
        ]);
        output(command, 1024)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let mut command = git(target.path());
        command.args(["init", "--bare", "-q"]);
        output(command, 1024)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let mut command = git(target.path());
        command.args([
            "fetch",
            "--no-tags",
            "--no-recurse-submodules",
            &relay.url,
            &format!("{pinned}:refs/heads/develop"),
            &format!("{tag_oid}:refs/tags/v1"),
        ]);
        output(command, 65536)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let mut command = git(target.path());
        command.args(["rev-parse", "refs/heads/develop"]);
        ensure!(
            String::from_utf8(
                output(command, 1024)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e:?}"))?
            )?
            .trim()
                == pinned
        );
        let mut command = git(target.path());
        command.args(["fsck", "--full", "--no-dangling"]);
        output(command, 65536)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let mut command = git(target.path());
        command.args(["rev-list", "--count", "refs/heads/develop"]);
        ensure!(
            String::from_utf8(
                output(command, 1024)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e:?}"))?
            )?
            .trim()
                == "2"
        );
        let mut command = git(target.path());
        command.args(["rev-parse", "refs/tags/v1"]);
        ensure!(
            String::from_utf8(
                output(command, 1024)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e:?}"))?
            )?
            .trim()
                == tag_oid
        );
        // Aggregate transfer limits apply even to a successful HTTPS response.
        let limited =
            Relay::with_client(source_origin.url.clone(), source_origin.client.clone(), 8)
                .await
                .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let mut command = git(target.path());
        command.args(["ls-remote", &limited.url]);
        let failure = output(command, 65536)
            .await
            .err()
            .context("transfer must fail")?;
        ensure!(matches!(limited.error(failure), ApiError::PayloadTooLarge));
        // A successful HTTP response can still be damaged by a transient
        // proxy/network path. Unknown Git failures must not become terminal409.
        let attempts = Arc::new(AtomicU64::new(0));
        let retry_path = source.path().to_path_buf();
        let counter = attempts.clone();
        let retry_router = Router::new()
            .route(
                "/source/info/refs",
                get(move |request: Request<Body>| {
                    let counter = counter.clone();
                    let path = retry_path.clone();
                    async move {
                        if counter.fetch_add(1, Ordering::Relaxed) == 0 {
                            (
                                [(
                                    "content-type",
                                    "application/x-git-upload-pack-advertisement",
                                )],
                                "damaged advertisement",
                            )
                                .into_response()
                        } else {
                            smart(State(path), request).await
                        }
                    }
                }),
            )
            .with_state(source.path().to_path_buf());
        let retry_router = retry_router.route(
            "/source/git-upload-pack",
            post({
                let path = source.path().to_path_buf();
                move |request: Request<Body>| {
                    let path = path.clone();
                    async move { smart(State(path), request).await }
                }
            }),
        );
        let retry_origin = origin(retry_router).await?;
        let retry_relay = Relay::with_client(
            retry_origin.url.clone(),
            retry_origin.client.clone(),
            1024 * 1024,
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let mut command = git(target.path());
        command.args(["ls-remote", &retry_relay.url]);
        let first = output(command, 65536)
            .await
            .err()
            .context("first source attempt should fail")?;
        ensure!(matches!(
            retry_relay.error(first),
            ApiError::ImportUnavailable(_)
        ));
        let mut command = git(target.path());
        command.args(["ls-remote", &retry_relay.url]);
        ensure!(
            !output(command, 65536)
                .await
                .map_err(|e| anyhow::anyhow!("{e:?}"))?
                .is_empty()
        );
        // Redirects toward a private/metadata destination are never followed.
        let hits = Arc::new(AtomicU64::new(0));
        let counter = hits.clone();
        let redirects = Router::new().route(
            "/source/info/refs",
            get(move |headers: axum::http::HeaderMap| {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::Relaxed);
                    assert!(headers.get("authorization").is_none());
                    assert!(headers.get("cookie").is_none());
                    (
                        StatusCode::FOUND,
                        [("location", "http://169.254.169.254/latest/meta-data")],
                    )
                }
            }),
        );
        let blocked = origin(redirects).await?;
        let relay = Relay::with_client(blocked.url.clone(), blocked.client.clone(), 1024)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let response = reqwest::Client::new()
            .get(format!("{}/info/refs?service=git-upload-pack", relay.url))
            .header("authorization", "Bearer must-not-escape")
            .header("cookie", "must-not-escape")
            .send()
            .await?;
        ensure!(response.status() == 422 && hits.load(Ordering::Relaxed) == 1);
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod pipe_tests {
    use super::*;
    #[tokio::test]
    async fn output_overflow_interrupts_a_producer_without_waiting_for_eof() -> anyhow::Result<()> {
        let mut producer = tokio::process::Command::new("/usr/bin/yes");
        producer
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(1), output(producer, 128)).await?;
        assert!(matches!(result, Err(ApiError::PayloadTooLarge)));
        Ok(())
    }
}
