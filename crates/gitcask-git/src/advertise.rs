use std::process::Stdio;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::process::Command;
use tracing::Instrument;

use crate::{
    GitError, LocalRepo, LsRefsArgs, LsRefsLine, ObjectFormat, Ref, Service, UploadPackRequest, pkt,
};

const GITCASK_VERSION: &str = env!("CARGO_PKG_VERSION");

impl LocalRepo {
    /// Raw passthrough: `git upload-pack --stateless-rpc` with `GIT_PROTOCOL`
    /// set from `protocol`. `body` is the request, `out` receives the response.
    pub async fn upload_pack_raw<R, W>(
        &self,
        protocol: pkt::Protocol,
        body: R,
        out: W,
    ) -> Result<(), GitError>
    where
        R: AsyncRead + Unpin + Send,
        W: AsyncWrite + Unpin + Send,
    {
        let span = tracing::info_span!(
            "git.upload_pack",
            repo = %self.inner.id,
            engine = "git",
        );
        self.run_upload_pack_stateless_io(protocol, body, out)
            .instrument(span.clone())
            .await
    }

    pub fn ls_refs(&self, args: &LsRefsArgs) -> Result<Vec<LsRefsLine>, GitError> {
        let span = tracing::debug_span!(
            "git.ls_refs",
            repo = %self.inner.id,
            prefixes = args.ref_prefixes.len(),
        );
        let _enter = span.enter();

        let snap = self.refs_arc()?;
        let mut lines = Vec::new();
        let head_target = snap.head_target.clone();
        // Resolve HEAD's target before filtering: a ref-prefix that excludes the
        // target ref must not prevent HEAD itself from being advertised.
        let head_oid = if head_target.is_empty() {
            None
        } else {
            snap.refs
                .binary_search_by(|r| r.name.as_str().cmp(head_target.as_str()))
                .ok()
                .map(|i| snap.refs[i].oid.clone())
        };
        // Prefix selection is O(log n + k) over the name-sorted list: each
        // prefix is one range (binary search for its start, scan while it
        // matches); ranges are merged so overlapping prefixes emit once.
        let selected: Vec<&Ref> = if args.ref_prefixes.is_empty() {
            snap.refs.iter().collect()
        } else {
            let mut ranges: Vec<(usize, usize)> = args
                .ref_prefixes
                .iter()
                .map(|p| {
                    let start = snap.refs.partition_point(|r| r.name.as_str() < p.as_str());
                    let mut end = start;
                    while end < snap.refs.len() && snap.refs[end].name.starts_with(p.as_str()) {
                        end += 1;
                    }
                    (start, end)
                })
                .collect();
            ranges.sort_unstable();
            let mut out = Vec::new();
            let mut cursor = 0usize;
            for (a, b) in ranges {
                let a = a.max(cursor);
                if a < b {
                    out.extend(snap.refs[a..b].iter());
                    cursor = b;
                }
            }
            out
        };
        for r in selected {
            if r.name == "HEAD" {
                continue; // rendered below from head_target/head_oid
            }
            lines.push(LsRefsLine {
                name: r.name.clone(),
                oid: r.oid.clone(),
                peeled: r.peeled.clone(),
                symref_target: None,
            });
        }
        // HEAD is advertised whenever a prefix matches it (empty prefixes match
        // all). `symrefs` only controls the `symref-target:` attribute, except for
        // the unborn form which always carries it (protocol-v2 ls-refs).
        let head_matches = args.ref_prefixes.is_empty()
            || args
                .ref_prefixes
                .iter()
                .any(|p| "HEAD".starts_with(p.as_str()));
        if head_matches && !head_target.is_empty() {
            match head_oid {
                Some(oid) => lines.push(LsRefsLine {
                    name: "HEAD".to_string(),
                    oid,
                    peeled: String::new(),
                    symref_target: Some(head_target.clone()),
                }),
                None if args.unborn => lines.push(LsRefsLine {
                    name: "HEAD".to_string(),
                    oid: "unborn".to_string(),
                    peeled: String::new(),
                    symref_target: Some(head_target.clone()),
                }),
                None => {}
            }
        }
        Ok(lines)
    }

    /// v0 advertisement with capabilities. The HTTP server prepends the
    /// `# service=<svc>\n` pkt-line + flush.
    pub fn advertise_refs_v0(&self, service: Service, out: &mut Vec<u8>) -> Result<(), GitError> {
        let snap = self.refs()?;
        let caps = capabilities_for(service, self.inner.format);
        let caps_line = format!("\0{}\n", caps);

        if snap.refs.is_empty() {
            // No refs: emit the capabilities line with a zero id and
            // `capabilities^{}`.
            let zero = zero_hex(self.inner.format);
            let line = format!("{zero} capabilities^{{}}{caps_line}");
            pkt::encode_data(out, line.as_bytes());
        } else {
            let head_target = snap.head_target;
            let mut first = true;
            for r in &snap.refs {
                let mut line = format!("{} {}", r.oid, r.name);
                if first {
                    line.push_str(&caps_line);
                    first = false;
                } else {
                    line.push('\n');
                }
                pkt::encode_data(out, line.as_bytes());
                // Peeled annotated tags (`<oid> refs/tags/x^{}`), like git's
                // `packed-refs`-backed advertisement.
                if !r.peeled.is_empty() && r.peeled != r.oid {
                    let peeled = format!("{} {}^{{}}\n", r.peeled, r.name);
                    pkt::encode_data(out, peeled.as_bytes());
                }
            }
            // Include HEAD if it has a resolvable target and isn't already the
            // first advertised ref (upload-pack advertises HEAD).
            if !head_target.is_empty() && service == Service::UploadPack {
                if let Some(oid) = snap
                    .refs
                    .iter()
                    .find(|r| r.name == head_target)
                    .map(|r| r.oid.clone())
                {
                    let head_line = format!("{oid} HEAD\n");
                    pkt::encode_data(out, head_line.as_bytes());
                }
            }
        }
        pkt::encode_flush(out);
        Ok(())
    }
    async fn run_upload_pack_stateless_io<R, W>(
        &self,
        protocol: pkt::Protocol,
        body: R,
        mut out: W,
    ) -> Result<(), GitError>
    where
        R: AsyncRead + Unpin + Send,
        W: AsyncWrite + Unpin + Send,
    {
        let mut body = body;
        let git_protocol = protocol.git_protocol_env();
        let mut cmd = Command::new("git");
        cmd.current_dir(&self.inner.path)
            .env("GIT_DIR", &self.inner.path)
            .env("GIT_PROTOCOL", git_protocol)
            // sideband-all: the server advertises it so it can narrate before
            // the packfile section; upload-pack only honours the client's
            // request with this config (also set at init, -c covers old copies).
            .args([
                "-c",
                "uploadpack.allowSidebandAll=true",
                "upload-pack",
                "--stateless-rpc",
                ".",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(GitError::Io)?;
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        // Copy the request body into stdin first, then close stdin so the
        // subprocess sees EOF and can finish + exit. Only then drain stdout:
        // `copy_out` blocks on stdout EOF (subprocess exit), and the subprocess
        // won't exit until stdin is closed, so they must NOT be joined
        // concurrently (that deadlocks).
        let copy_in = tokio::io::copy(&mut body, &mut stdin);
        let in_res = copy_in.await;
        drop(stdin);
        in_res.map_err(GitError::Io)?;
        let out_res = tokio::io::copy(&mut stdout, &mut out).await;
        out_res.map_err(GitError::Io)?;
        let status = child.wait().await.map_err(GitError::Io)?;
        if !status.success() {
            let stderr = child.stderr.take();
            if let Some(mut e) = stderr {
                let mut s = String::new();
                let _ = e.read_to_string(&mut s).await;
                return Err(GitError::Subprocess {
                    cmd: "git upload-pack".into(),
                    status: status.code(),
                    stderr: s,
                });
            }
            return Err(GitError::Subprocess {
                cmd: "git upload-pack".into(),
                status: status.code(),
                stderr: String::new(),
            });
        }
        Ok(())
    }
}

fn zero_hex(format: ObjectFormat) -> String {
    "0".repeat(match format {
        ObjectFormat::Sha1 => 40,
        ObjectFormat::Sha256 => 64,
    })
}

fn capabilities_for(service: Service, format: ObjectFormat) -> String {
    let of = format.as_str();
    let agent = format!("agent=gitcask/{GITCASK_VERSION}");
    match service {
        Service::UploadPack => format!(
            "multi_ack_detailed side-band-64k thin-pack ofs-delta shallow deepen-since deepen-not \
             no-progress include-tag allow-tip-sha1-in-want allow-reachable-sha1-in-want filter \
             object-format={of} {agent}"
        ),
        Service::ReceivePack => format!(
            "report-status report-status-v2 delete-refs side-band-64k quiet atomic ofs-delta \
             push-options object-format={of} {agent}"
        ),
    }
}
/// Build the v2 fetch command pkt-line request bytes from a typed request.
pub fn build_v2_fetch_request(req: &UploadPackRequest) -> Vec<u8> {
    let mut buf = Vec::new();
    pkt::encode_data(&mut buf, b"command=fetch\n");
    // Git protocol v2 carries all fetch features (thin-pack, want, have, ...)
    // as arguments following the delim-pkt; there is no pre-delim capability
    // section for fetch.
    pkt::encode_delim(&mut buf);
    if req.thin_pack {
        pkt::encode_data(&mut buf, b"thin-pack\n");
    }
    if req.ofs_delta {
        pkt::encode_data(&mut buf, b"ofs-delta\n");
    }
    if req.no_progress {
        pkt::encode_data(&mut buf, b"no-progress\n");
    }
    if req.include_tag {
        pkt::encode_data(&mut buf, b"include-tag\n");
    }
    if req.sideband_all {
        pkt::encode_data(&mut buf, b"sideband-all\n");
    }
    if req.wait_for_done {
        pkt::encode_data(&mut buf, b"wait-for-done\n");
    }
    if let Some(f) = &req.filter {
        pkt::encode_data(&mut buf, format!("filter {f}\n").as_bytes());
    }
    for w in &req.wants {
        pkt::encode_data(&mut buf, format!("want {}\n", w.to_hex()).as_bytes());
    }
    for h in &req.haves {
        pkt::encode_data(&mut buf, format!("have {}\n", h.to_hex()).as_bytes());
    }
    for s in &req.shallow {
        pkt::encode_data(&mut buf, format!("shallow {}\n", s.to_hex()).as_bytes());
    }
    if let Some(d) = req.deepen {
        pkt::encode_data(&mut buf, format!("deepen {d}\n").as_bytes());
    }
    if let Some(ts) = req.deepen_since {
        pkt::encode_data(&mut buf, format!("deepen-since {ts}\n").as_bytes());
    }
    for n in &req.deepen_not {
        pkt::encode_data(&mut buf, format!("deepen-not {n}\n").as_bytes());
    }
    for r in &req.want_refs {
        pkt::encode_data(&mut buf, format!("want-ref {r}\n").as_bytes());
    }
    if req.done {
        pkt::encode_data(&mut buf, b"done\n");
    }
    pkt::encode_flush(&mut buf);
    buf
}
