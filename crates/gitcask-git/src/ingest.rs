use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tracing::Instrument;

use crate::proc::rename_atomic;
use crate::{GitError, IngestOptions, IngestedPack, LocalRepo, PackInfo, write_rev_from_idx};

impl LocalRepo {
    fn objects_pack_dir(&self) -> PathBuf {
        self.inner.path.join("objects").join("pack")
    }

    // ---- packs ----

    /// Stream a packfile in, index it with `git index-pack`, and install
    /// `pack-<checksum>.{pack,idx,rev}` into `objects/pack/`. Thin packs
    /// (`opts.thin`, every receive-pack) use `--fix-thin` against this
    /// repo's ODB. Empty input returns Ok(None). `opts.fsck` adds
    /// `--fsck-objects` so object parse happens in the same pass as
    /// indexing (a large repository: 64 k objects used to spend tens of seconds in a
    /// second gix walk after a gix write).
    pub async fn ingest_pack<R: AsyncRead + Unpin + Send + 'static>(
        &self,
        mut pack: R,
        opts: IngestOptions,
    ) -> Result<Option<IngestedPack>, GitError> {
        let span = tracing::info_span!(
            "git.ingest_pack",
            repo = %self.inner.id,
            objects = 0u64,
            bytes = 0u64,
            engine = "git",
            thin = opts.thin,
            feed_ms = 0u64,
            git_ms = 0u64,
        );
        // Instrument each awaited operation rather than carrying a thread-local
        // span guard across await points.
        // Pack installation and repository refresh are not safe concurrently
        // with gix's pack/index readers. Serialize ingestion per repository;
        // callers may still run ingests for different repositories in parallel.
        let _ingest_guard = self.inner.ingest_lock.lock().instrument(span.clone()).await;

        let pack_dir = self.objects_pack_dir();
        std::fs::create_dir_all(&pack_dir).map_err(GitError::Io)?;

        // Reserve the temporary path atomically. A timestamp suffix alone can
        // collide when many ingest calls start in the same scheduler tick.
        let (tmp_path, tmp_file) = loop {
            let candidate = pack_dir.join(format!("tmp-ingest-{}.pack", unique_suffix()));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(file) => break (candidate, file),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(GitError::Io(e)),
            }
        };
        let mut tmp = tokio::fs::File::from_std(tmp_file);
        let mut total: u64 = 0;
        let mut buf = vec![0u8; 64 * 1024];
        let mut empty_check = true;
        loop {
            let n = pack
                .read(&mut buf)
                .instrument(span.clone())
                .await
                .map_err(GitError::Io)?;
            if n == 0 {
                break;
            }
            empty_check = false;
            total += n as u64;
            if let Some(max) = opts.max_bytes {
                if total > max {
                    drop(
                        tokio::fs::remove_file(&tmp_path)
                            .instrument(span.clone())
                            .await,
                    );
                    return Err(GitError::InvalidInput(format!(
                        "pack exceeds max_bytes {max}"
                    )));
                }
            }
            tmp.write_all(&buf[..n])
                .instrument(span.clone())
                .await
                .map_err(GitError::Io)?;
        }
        // tokio's File buffers writes in a background blocking task and does
        // NOT flush on drop: without this the tail of the pack may be missing
        // when index-pack reads it back (seen as "failed to fill whole buffer"
        // under load).
        tmp.flush()
            .instrument(span.clone())
            .await
            .map_err(GitError::Io)?;
        drop(tmp);
        span.record("bytes", total);
        if empty_check {
            let _ = tokio::fs::remove_file(&tmp_path)
                .instrument(span.clone())
                .await;
            return Ok(None);
        }

        // `git index-pack` is the receive-pack ingest engine: it is the tool
        // that `--fix-thin` + `--threads` + `--fsck-objects` + `--rev-index`
        // were built for. gix-pack 0.73's write path was the previous engine
        // (a second object walk for fsck, no `.rev`, and a parser hole that
        // already fell back here). A large repository: 64,317 objects / 75 MB in 49.1 s
        // on that path (2026-08-21).
        let repo_path = self.inner.path.clone();
        let tmp_for_index = tmp_path.clone();
        let fix_thin = opts.thin;
        let fsck = opts.fsck;
        let index_span = tracing::info_span!(
            parent: &span,
            "git.ingest_pack.index",
            feed_ms = 0u64,
            git_ms = 0u64,
            phases = tracing::field::Empty,
        );
        let indexed = tokio::task::spawn_blocking(move || {
            git_index_pack(&tmp_for_index, &repo_path, fix_thin, fsck)
        })
        .instrument(index_span.clone())
        .await
        .map_err(|e| GitError::Io(std::io::Error::other(e)));
        let _ = tokio::fs::remove_file(&tmp_path)
            .instrument(span.clone())
            .await;
        let outcome = indexed??;
        index_span.record("feed_ms", outcome.feed_ms);
        index_span.record("git_ms", outcome.git_ms);
        index_span.record("phases", outcome.phases.as_str());
        span.record("feed_ms", outcome.feed_ms);
        span.record("git_ms", outcome.git_ms);
        tracing::info!(
            parent: &index_span,
            feed_ms = outcome.feed_ms,
            git_ms = outcome.git_ms,
            phases = %outcome.phases,
            "git.index_pack.trace2"
        );
        let checksum = outcome.checksum;
        let pack_path = outcome.pack_path;
        let idx_path = outcome.idx_path;
        let object_count = outcome.object_count;
        span.record("objects", object_count);
        // A ref-only push (`git push origin main:feature` with nothing new)
        // sends a 32-byte pack with zero objects. Nothing to publish.
        if object_count == 0 {
            let _ = std::fs::remove_file(&pack_path);
            let _ = std::fs::remove_file(&idx_path);
            let _ = std::fs::remove_file(pack_path.with_extension("rev"));
            return Ok(None);
        }
        let pack_size = std::fs::metadata(&pack_path).map(|m| m.len()).unwrap_or(0);
        let idx_size = std::fs::metadata(&idx_path).map(|m| m.len()).unwrap_or(0);
        self.refresh_async()
            .instrument(tracing::info_span!(parent: &span, "git.ingest_pack.refresh"))
            .await?;
        Ok(Some(IngestedPack {
            checksum,
            pack_path,
            idx_path,
            pack_size,
            idx_size,
            object_count,
        }))
    }

    /// Atomically move downloaded files into `objects/pack/`, then refresh.
    pub async fn install_pack(
        &self,
        pack: &Path,
        idx: &Path,
        extra: &[PathBuf],
    ) -> Result<(), GitError> {
        let this = self.clone();
        let pack = pack.to_path_buf();
        let idx = idx.to_path_buf();
        let extra = extra.to_vec();
        tokio::task::spawn_blocking(move || {
            let pack_dir = this.objects_pack_dir();
            std::fs::create_dir_all(&pack_dir).map_err(GitError::Io)?;
            let dst_pack = pack_dir.join(pack.file_name().unwrap());
            let dst_idx = pack_dir.join(idx.file_name().unwrap());
            rename_atomic(&pack, &dst_pack)?;
            rename_atomic(&idx, &dst_idx)?;
            for path in extra {
                let dst = pack_dir.join(path.file_name().unwrap());
                rename_atomic(&path, &dst)?;
            }
            Ok::<(), GitError>(())
        })
        .await
        .map_err(|error| GitError::Protocol(format!("pack install task: {error}")))??;
        self.refresh_async().await?;
        Ok(())
    }

    /// Write `pack-<checksum>.rev` for an installed pack **from its `.idx`
    /// alone** (no pack bytes read: `git index-pack --rev-index` re-indexes
    /// the whole pack — 4 GB of a large repository's 32 GB in 52 min, 2026-08-21). Without
    /// a `.rev` git rebuilds the reverse index in memory on EVERY
    /// `pack-objects`: a large repository's 60 M-object base cost 2.85 s per fetch
    /// (the original large-repository measurements). The file is a bucket side-file like `.bitmap`:
    /// the caller uploads it (`RepoHandle::annotate_pack`) so the fleet
    /// converges once. Returns the path; a no-op when it already exists.
    pub async fn write_rev_index(&self, checksum: &gix_hash::oid) -> Result<PathBuf, GitError> {
        let rev = self.pack_path(checksum).with_extension("rev");
        if rev.exists() {
            return Ok(rev);
        }
        let idx = self.pack_path(checksum).with_extension("idx");
        let kind = self.object_format().kind();
        let rev_out = rev.clone();
        tokio::task::spawn_blocking(move || write_rev_from_idx(&idx, &rev_out, kind))
            .await
            .map_err(|e| GitError::InvalidInput(format!("rev index task: {e}")))??;
        self.refresh_async().await?;
        Ok(rev)
    }

    /// Delete `.pack/.idx/.rev/.bitmap` for `checksum`. Caller guarantees no
    /// readers.
    pub fn remove_pack(&self, checksum: &gix_hash::oid) -> Result<(), GitError> {
        let hex = checksum.to_hex();
        let pack_dir = self.objects_pack_dir();
        for ext in ["pack", "idx", "rev", "bitmap", "commit-graph"] {
            let p = pack_dir.join(format!("pack-{hex}.{ext}"));
            match std::fs::remove_file(&p) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(GitError::Io(e)),
            }
        }
        Ok(())
    }

    pub fn packs(&self) -> Result<Vec<PackInfo>, GitError> {
        let pack_dir = self.objects_pack_dir();
        let mut out = Vec::new();
        let rd = match std::fs::read_dir(&pack_dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(GitError::Io(e)),
        };
        for ent in rd {
            let ent = ent.map_err(GitError::Io)?;
            let name = ent.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with("pack-") || !name.ends_with(".pack") {
                continue;
            }
            let hex = &name["pack-".len()..name.len() - ".pack".len()];
            let checksum = match gix_hash::ObjectId::from_hex(hex.as_bytes()) {
                Ok(o) => o,
                Err(_) => continue,
            };
            let pack_path = ent.path();
            let idx_path = pack_path.with_extension("idx");
            let pack_size = std::fs::metadata(&pack_path).map(|m| m.len()).unwrap_or(0);
            let idx_size = std::fs::metadata(&idx_path).map(|m| m.len()).unwrap_or(0);
            let object_count = idx_object_count(&idx_path).unwrap_or(0);
            let has_rev = pack_path.with_extension("rev").exists();
            let has_bitmap = pack_path.with_extension("bitmap").exists();
            let has_commit_graph = pack_path.with_extension("commit-graph").exists();
            out.push(PackInfo {
                checksum,
                pack_size,
                idx_size,
                object_count,
                has_rev,
                has_bitmap,
                has_commit_graph,
            });
        }
        out.sort_by_key(|p| p.checksum);
        Ok(out)
    }

    pub fn pack_path(&self, checksum: &gix_hash::oid) -> PathBuf {
        let hex = checksum.to_hex();
        self.objects_pack_dir().join(format!("pack-{hex}.pack"))
    }
}

fn unique_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{}", std::process::id(), nanos)
}
fn idx_object_count(idx_path: &Path) -> Result<u64, GitError> {
    use std::io::{Read, Seek};
    let mut f = std::fs::File::open(idx_path).map_err(GitError::Io)?;
    let mut head = [0u8; 8];
    f.read_exact(&mut head).map_err(GitError::Io)?;
    let is_v2 = &head[..4] == b"\xfftOc";
    let fanout_off = if is_v2 { 8 + 255 * 4 } else { 255 * 4 };
    f.seek(std::io::SeekFrom::Start(fanout_off as u64))
        .map_err(GitError::Io)?;
    let mut buf = [0u8; 4];
    f.read_exact(&mut buf).map_err(GitError::Io)?;
    Ok(u32::from_be_bytes(buf) as u64)
}
struct IndexPackOutcome {
    checksum: gix_hash::ObjectId,
    pack_path: PathBuf,
    idx_path: PathBuf,
    object_count: u64,
    /// Time spent copying the pack into index-pack stdin.
    feed_ms: u64,
    /// `exit.t_abs` from GIT_TRACE2_EVENT (whole child). index-pack itself
    /// emits no region_leave events today; any that appear (future git) are
    /// in `phases`.
    git_ms: u64,
    /// Compact `k=ms` list: always `feed` + `git`, plus every TRACE2
    /// `region_leave` (`category:label`).
    phases: String,
}

fn git_index_pack(
    input: &Path,
    repo_path: &Path,
    fix_thin: bool,
    fsck: bool,
) -> Result<IndexPackOutcome, GitError> {
    let file = std::fs::File::open(input).map_err(GitError::Io)?;
    // `--threads=0` = auto (ncpus). `--rev-index` writes `.rev` in the same
    // pass so the next pack-objects does not rebuild a reverse index in RAM.
    // `--fsck-objects` parses commits/trees/tags while resolving — one walk,
    // not a second pass over the new pack through the whole ODB.
    let mut args = vec![
        "index-pack",
        "--stdin",
        "--keep",
        "--rev-index",
        "--threads=0",
    ];
    if fix_thin {
        args.push("--fix-thin");
    }
    if fsck {
        args.push("--fsck-objects");
    }
    let suffix = unique_suffix();
    let trace_path = std::env::temp_dir().join(format!("gitcask-index-pack-{suffix}.jsonl"));
    // index-pack runs in a per-ingest scratch git dir whose objects/info/alternates points at the
    // repository: `--fix-thin` and `--fsck-objects` see the ODB, but everything index-pack writes —
    // the finished pack/idx/rev and, on failure, the `tmp_pack_*` it does not clean up (one
    // pack-sized leak per rejected push, on tmpfs) — lands under the scratch dir, which is removed
    // whole; the finished files are renamed into objects/pack (same filesystem, atomic).
    let scratch = repo_path.join(format!("gitcask-ingest-{suffix}"));
    let _scratch_guard = ScratchDir(scratch.clone());
    let scratch_pack_dir = scratch.join("objects").join("pack");
    std::fs::create_dir_all(&scratch_pack_dir).map_err(GitError::Io)?;
    std::fs::create_dir_all(scratch.join("objects").join("info")).map_err(GitError::Io)?;
    std::fs::create_dir_all(scratch.join("refs")).map_err(GitError::Io)?;
    std::fs::write(scratch.join("HEAD"), "ref: refs/heads/main\n").map_err(GitError::Io)?;
    // The repository's config: object format (sha256 repos), repositoryformatversion, pack knobs.
    std::fs::copy(repo_path.join("config"), scratch.join("config")).map_err(GitError::Io)?;
    std::fs::write(
        scratch.join("objects").join("info").join("alternates"),
        format!("{}\n", repo_path.join("objects").display()),
    )
    .map_err(GitError::Io)?;
    let mut child = std::process::Command::new("git")
        .current_dir(repo_path)
        .env("GIT_DIR", &scratch)
        .env("GIT_TRACE2_EVENT", &trace_path)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(GitError::Io)?;
    let feed_started = std::time::Instant::now();
    {
        let mut stdin = child.stdin.take().ok_or_else(|| {
            GitError::Io(std::io::Error::other("git index-pack stdin unavailable"))
        })?;
        let mut file = file;
        std::io::copy(&mut file, &mut stdin).map_err(GitError::Io)?;
    }
    let feed_ms = feed_started.elapsed().as_millis() as u64;
    let output = child.wait_with_output().map_err(GitError::Io)?;
    let trace = std::fs::read_to_string(&trace_path).unwrap_or_default();
    let _ = std::fs::remove_file(&trace_path);
    if !output.status.success() {
        return Err(GitError::Subprocess {
            cmd: "git index-pack".into(),
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    let checksum = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .find_map(|word| gix_hash::ObjectId::from_hex(word.as_bytes()).ok())
        .ok_or_else(|| GitError::Protocol("git index-pack returned no checksum".into()))?;
    let hex = checksum.to_hex();
    let pack_dir = repo_path.join("objects").join("pack");
    std::fs::create_dir_all(&pack_dir).map_err(GitError::Io)?;
    // Move the finished files home: idx and rev first, the pack last, so a reader that sees the
    // pack also finds its index. `.keep` only exists to protect a pack until refs point at it;
    // publish is our commit point, and leaving it would hide the pack from `repack -d`.
    for ext in ["idx", "rev", "pack"] {
        let from = scratch_pack_dir.join(format!("pack-{hex}.{ext}"));
        if from.exists() {
            rename_atomic(&from, &pack_dir.join(format!("pack-{hex}.{ext}")))?;
        }
    }
    let pack_path = pack_dir.join(format!("pack-{hex}.pack"));
    let idx_path = pack_dir.join(format!("pack-{hex}.idx"));
    let object_count = idx_object_count(&idx_path)?;
    let parsed = parse_index_pack_trace2(&trace);
    let git_ms = parsed.git_ms;
    let phases = format_index_pack_phases(feed_ms, git_ms, &parsed.regions);
    Ok(IndexPackOutcome {
        checksum,
        pack_path,
        idx_path,
        object_count,
        feed_ms,
        git_ms,
        phases,
    })
}

/// Removes the per-ingest scratch git dir on every exit path (success, refusal, panic).
struct ScratchDir(PathBuf);
impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Trace2Phases {
    git_ms: u64,
    regions: Vec<(String, u64)>,
}

fn secs_to_ms(t: f64) -> u64 {
    let ms = (t * 1000.0).ceil() as u64;
    if t > 0.0 && ms == 0 { 1 } else { ms }
}

/// Pull a JSON string field (`"key":"value"`) out of a TRACE2 event line.
fn json_str_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\":\"");
    let rest = line.split_once(&pat)?.1;
    let end = rest.find('"')?;
    Some(&rest[..end])
}

fn json_f64_field(line: &str, key: &str) -> Option<f64> {
    let pat = format!("\"{key}\":");
    let rest = line.split_once(&pat)?.1.trim_start();
    let token = rest.split([',', '}', ' ']).next()?.trim();
    token.parse().ok()
}

fn parse_index_pack_trace2(text: &str) -> Trace2Phases {
    let mut git_ms = 0u64;
    let mut regions = Vec::new();
    for line in text.lines() {
        let event = json_str_field(line, "event").unwrap_or("");
        match event {
            "exit" => {
                if let Some(t) = json_f64_field(line, "t_abs") {
                    git_ms = secs_to_ms(t);
                }
            }
            "region_leave" => {
                let label = json_str_field(line, "label")
                    .or_else(|| json_str_field(line, "name"))
                    .unwrap_or("");
                if label.is_empty() {
                    continue;
                }
                let name = match json_str_field(line, "category") {
                    Some(c) if !c.is_empty() => format!("{c}:{label}"),
                    _ => label.to_string(),
                };
                let t = json_f64_field(line, "t_rel").or_else(|| json_f64_field(line, "t_abs"));
                if let Some(t) = t {
                    regions.push((name, secs_to_ms(t)));
                }
            }
            _ => {}
        }
    }
    Trace2Phases { git_ms, regions }
}

fn format_index_pack_phases(feed_ms: u64, git_ms: u64, regions: &[(String, u64)]) -> String {
    let mut out = format!("feed={feed_ms},git={git_ms}");
    for (name, ms) in regions {
        out.push(',');
        out.push_str(name);
        out.push('=');
        out.push_str(&ms.to_string());
    }
    out
}
#[cfg(test)]
mod index_pack_trace_tests {
    use super::*;

    #[test]
    fn parse_trace2_exit_and_region_leave() {
        let sample = r#"
{"event":"start","t_abs":0.0001}
{"event":"region_leave","category":"index-pack","label":"resolve_deltas","t_rel":1.5}
{"event":"region_leave","category":"index-pack","label":"fsck","t_rel":0.25}
{"event":"exit","t_abs":2.251,"code":0}
"#;
        let p = parse_index_pack_trace2(sample);
        assert_eq!(p.git_ms, 2251);
        assert_eq!(
            p.regions,
            vec![
                ("index-pack:resolve_deltas".into(), 1500),
                ("index-pack:fsck".into(), 250),
            ]
        );
        let phases = format_index_pack_phases(3, p.git_ms, &p.regions);
        assert_eq!(
            phases,
            "feed=3,git=2251,index-pack:resolve_deltas=1500,index-pack:fsck=250"
        );
    }

    #[test]
    fn successful_index_pack_records_non_zero_git_ms() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let run = |args: &[&str]| {
            let o = std::process::Command::new("git")
                .current_dir(&src)
                .args(args)
                .output()
                .unwrap();
            assert!(
                o.status.success(),
                "{:?} {}",
                args,
                String::from_utf8_lossy(&o.stderr)
            );
            o
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(src.join("f"), "hello\n").unwrap();
        run(&["add", "f"]);
        run(&["commit", "-qm", "c"]);
        let pack = {
            let mut child = std::process::Command::new("git")
                .current_dir(&src)
                .args(["pack-objects", "--stdout", "--revs"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            {
                let mut stdin = child.stdin.take().unwrap();
                use std::io::Write;
                stdin.write_all(b"HEAD\n").unwrap();
            }
            let out = child.wait_with_output().unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            out.stdout
        };
        let dest = dir.path().join("dest.git");
        let o = std::process::Command::new("git")
            .args(["init", "-q", "--bare", dest.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(o.status.success());
        let input = dir.path().join("in.pack");
        std::fs::write(&input, &pack).unwrap();
        let outcome = git_index_pack(&input, &dest, false, true).expect("index-pack");
        assert!(outcome.object_count > 0);
        assert!(
            outcome.git_ms > 0 || outcome.feed_ms > 0,
            "expected a phase: {}",
            outcome.phases
        );
        assert!(
            outcome.phases.contains("git="),
            "phases must name git_ms: {}",
            outcome.phases
        );
        assert!(outcome.pack_path.exists());
        assert!(outcome.idx_path.exists());
    }
}
