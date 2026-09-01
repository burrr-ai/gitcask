use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use crate::proc::rename_atomic;
use crate::{GitError, LocalRepo, ObjectFormat, Ref, RefSnapshotData, RepoId, validate_ref_update};

/// Point lookups over a shared, name-sorted ref snapshot plus an overlay of
/// pending changes: O(log n) instead of building an O(n) `HashMap` per push
/// (500 k refs = a 34 MB map per push on both the verify and the publish path,
/// 2026-08-21). The overlay holds what a batch applied so far.
pub struct RefView {
    base: Arc<RefSnapshotData>,
    overlay: HashMap<String, Option<String>>,
    head_target: Option<String>,
}

impl RefView {
    pub fn new(base: Arc<RefSnapshotData>) -> Self {
        Self {
            base,
            overlay: HashMap::new(),
            head_target: None,
        }
    }
    /// Current oid (or symbolic target for symrefs) of `name`; `None` = absent.
    pub fn get(&self, name: &str) -> Option<String> {
        if let Some(v) = self.overlay.get(name) {
            return v.clone();
        }
        if name == "HEAD" {
            return self.head_oid();
        }
        self.base
            .refs
            .binary_search_by(|r| r.name.as_str().cmp(name))
            .ok()
            .map(|i| self.base.refs[i].oid.clone())
    }
    pub fn head_target(&self) -> &str {
        self.head_target
            .as_deref()
            .unwrap_or(&self.base.head_target)
    }
    /// HEAD's symbolic target as of the overlay (a pending `HEAD` symref update).
    pub fn set_head_target(&mut self, target: String) {
        self.head_target = Some(target);
    }
    /// HEAD's oid through its symbolic target (None when unborn/detached-empty).
    pub fn head_oid(&self) -> Option<String> {
        let target = self.head_target().to_string();
        if target.is_empty() {
            return None;
        }
        self.get(&target)
    }
    pub fn set(&mut self, name: &str, value: String) {
        self.overlay.insert(name.to_string(), Some(value));
    }
    pub fn remove(&mut self, name: &str) {
        self.overlay.insert(name.to_string(), None);
    }
}

pub(crate) struct Inner {
    pub(crate) id: RepoId,
    pub(crate) path: PathBuf,
    pub(crate) format: ObjectFormat,
    pub(crate) tsr: parking_lot::Mutex<gix::ThreadSafeRepository>,
    pub(crate) ingest_lock: tokio::sync::Mutex<()>,
    /// Parsed refs, shared by every reader until a ref write or a change of
    /// `packed-refs`/`HEAD` on disk (see [`LocalRepo::refs_arc`]).
    pub(crate) refs_cache: parking_lot::Mutex<Option<RefsCached>>,
    /// Bumped by every ref writer in this process; part of the cache key.
    pub(crate) refs_gen: std::sync::atomic::AtomicU64,
    /// How often `packed-refs` + loose refs were parsed (tests assert pushes do not add to it).
    pub(crate) refs_parses: std::sync::atomic::AtomicU64,
}

#[derive(Clone)]
pub(crate) struct RefsCached {
    key: RefsKey,
    /// The parsed (or last materialized) snapshot.
    data: Arc<RefSnapshotData>,
    /// Ref transactions this process applied since `data` was built, not yet folded in: a push
    /// records its txn here in O(k) and the next *reader* that needs the full sorted vector
    /// pays one O(n) copy for all of them ([`LocalRepo::refs_arc`]); lookups ([`LocalRepo::ref_view`])
    /// never materialize.
    pending: Vec<gitcask_proto::v1::RefTransaction>,
}

#[derive(PartialEq, Eq, Clone, Copy)]
struct RefsKey {
    generation: u64,
    packed_len: u64,
    packed_mtime: Option<std::time::SystemTime>,
    head_mtime: Option<std::time::SystemTime>,
}

fn refs_key(path: &Path, generation: u64) -> RefsKey {
    let packed = std::fs::metadata(path.join("packed-refs")).ok();
    let head = std::fs::metadata(path.join("HEAD")).ok();
    RefsKey {
        generation,
        packed_len: packed.as_ref().map(|m| m.len()).unwrap_or(0),
        packed_mtime: packed.and_then(|m| m.modified().ok()),
        head_mtime: head.and_then(|m| m.modified().ok()),
    }
}

impl LocalRepo {
    pub fn refs(&self) -> Result<RefSnapshotData, GitError> {
        Ok((*self.refs_arc()?).clone())
    }

    /// The refs, parsed once and shared: `packed-refs` of a 500 k-ref repo is
    /// 34 MB and read_refs also peels every tag — 1–2 s per call, which every
    /// `ls-refs` (prefix or not) paid (2026-08-21, test/refs500k on a serverless host).
    /// Valid until a ref writer in this process bumps the generation or
    /// `packed-refs`/`HEAD` change on disk (two stats per call). Sorted by name.
    pub fn refs_arc(&self) -> Result<Arc<RefSnapshotData>, GitError> {
        let key = refs_key(
            &self.inner.path,
            self.inner
                .refs_gen
                .load(std::sync::atomic::Ordering::Acquire),
        );
        {
            let mut guard = self.inner.refs_cache.lock();
            if let Some(c) = guard.as_mut()
                && c.key == key
            {
                if !c.pending.is_empty() {
                    // Fold the pushes applied since the last materialization: one copy of the
                    // vector for all of them, no parsing, no object reads.
                    let t = std::time::Instant::now();
                    let patched = self.patch_snapshot(&c.data, &c.pending);
                    c.data = Arc::new(patched);
                    c.pending.clear();
                    if c.data.refs.len() >= 10_000 {
                        tracing::debug!(repo = %self.inner.id, refs = c.data.refs.len(), ms = t.elapsed().as_millis() as u64, "refs cache materialized from pending txns");
                    }
                }
                return Ok(c.data.clone());
            }
        }
        let t = std::time::Instant::now();
        let data = Arc::new(read_refs(&self.inner.path)?);
        self.inner
            .refs_parses
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if data.refs.len() >= 10_000 {
            tracing::debug!(repo = %self.inner.id, refs = data.refs.len(), ms = t.elapsed().as_millis() as u64, "refs parsed into the cache");
        }
        *self.inner.refs_cache.lock() = Some(RefsCached {
            key,
            data: data.clone(),
            pending: Vec::new(),
        });
        Ok(data)
    }

    /// Number of full ref parses so far (`packed-refs` + loose refs + tag peeling): the O(refs)
    /// cost a push must not incur (AGENTS §1.4).
    pub fn refs_parses(&self) -> u64 {
        self.inner
            .refs_parses
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Point lookups over the current refs without materializing: the cached snapshot plus an
    /// overlay of the transactions applied since (O(k)). What the push path uses for
    /// `check_old` and the publisher's working view — a push after a push costs O(k), not a
    /// 500 k-entry copy.
    pub fn ref_view(&self) -> Result<RefView, GitError> {
        let key = refs_key(
            &self.inner.path,
            self.inner
                .refs_gen
                .load(std::sync::atomic::Ordering::Acquire),
        );
        let cached = self
            .inner
            .refs_cache
            .lock()
            .as_ref()
            .filter(|c| c.key == key)
            .cloned();
        let Some(c) = cached else {
            return Ok(RefView::new(self.refs_arc()?));
        };
        let mut view = RefView::new(c.data);
        for txn in &c.pending {
            for u in &txn.updates {
                if !u.new_symbolic_target.is_empty() {
                    if u.name == "HEAD" {
                        view.set_head_target(u.new_symbolic_target.clone());
                    }
                    continue;
                }
                if u.new_oid.is_empty() || u.new_oid.chars().all(|c| c == '0') {
                    view.remove(&u.name);
                } else {
                    view.set(&u.name, u.new_oid.clone());
                }
            }
        }
        Ok(view)
    }

    /// Invalidate the refs cache (every ref writer in this file calls it).
    pub(crate) fn refs_changed(&self) {
        self.inner
            .refs_gen
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }

    pub fn apply_ref_txn(
        &self,
        txn: &gitcask_proto::v1::RefTransaction,
        check_old: bool,
    ) -> Result<(), GitError> {
        let span = tracing::info_span!(
            "git.apply_ref_txn",
            repo = %self.inner.id,
            n_updates = txn.updates.len(),
            check_old,
        );
        let _enter = span.enter();
        // The cached snapshot as of now (if current): patched and re-installed after the txn
        // instead of thrown away — see the end of this function. Taken before git touches
        // packed-refs (a delete of a packed ref rewrites it and would change the key).
        let before = {
            let key = refs_key(
                &self.inner.path,
                self.inner
                    .refs_gen
                    .load(std::sync::atomic::Ordering::Acquire),
            );
            self.inner
                .refs_cache
                .lock()
                .as_ref()
                .filter(|c| c.key == key)
                .cloned()
        };

        for u in &txn.updates {
            validate_ref_update(u)?;
        }

        // Pre-check old values for clear error reporting.
        if check_old {
            let view = self.ref_view()?;
            for u in &txn.updates {
                if !u.new_symbolic_target.is_empty() {
                    continue;
                }
                let current = view.get(&u.name).unwrap_or_default();
                let old = u.old_oid.trim_start_matches('0');
                let cur = current.trim_start_matches('0');
                if old.is_empty() {
                    // must not exist
                    if !cur.is_empty() {
                        return Err(GitError::RefConflict {
                            name: u.name.clone(),
                            expected: u.old_oid.clone(),
                            actual: current.to_string(),
                        });
                    }
                } else if old != cur {
                    return Err(GitError::RefConflict {
                        name: u.name.clone(),
                        expected: u.old_oid.clone(),
                        actual: current.to_string(),
                    });
                }
            }
        }

        // Build `git update-ref --stdin` transaction for oid updates. Symbolic
        // ref updates (HEAD target) are applied separately by writing the HEAD
        // file directly — the `symref` command is not universally available in
        // update-ref --stdin (e.g. older forks), so we avoid it.
        let mut input = String::new();
        let mut symref_updates: Vec<&gitcask_proto::v1::RefUpdate> = Vec::new();
        let mut has_oid_cmds = false;
        input.push_str("start\n");
        for u in &txn.updates {
            if !u.new_symbolic_target.is_empty() {
                symref_updates.push(u);
                continue;
            }
            has_oid_cmds = true;
            let new_zero = u.new_oid.chars().all(|c| c == '0') || u.new_oid.is_empty();
            let old_zero = u.old_oid.chars().all(|c| c == '0') || u.old_oid.is_empty();
            if new_zero {
                // delete
                if check_old && !old_zero {
                    input.push_str(&format!("delete {} {}\n", u.name, u.old_oid));
                } else {
                    input.push_str(&format!("delete {}\n", u.name));
                }
            } else if check_old && old_zero {
                input.push_str(&format!("create {} {}\n", u.name, u.new_oid));
            } else if check_old && !old_zero {
                input.push_str(&format!("update {} {} {}\n", u.name, u.new_oid, u.old_oid));
            } else {
                input.push_str(&format!("update {} {}\n", u.name, u.new_oid));
            }
        }

        if has_oid_cmds {
            input.push_str("prepare\ncommit\n");
            let out = std::process::Command::new("git")
                .current_dir(&self.inner.path)
                .env("GIT_DIR", &self.inner.path)
                .args(["update-ref", "--stdin"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .and_then(|mut c| {
                    {
                        let stdin = c.stdin.as_mut().unwrap();
                        stdin.write_all(input.as_bytes())?;
                    }
                    c.wait_with_output()
                })
                .map_err(GitError::Io)?;

            if !out.status.success() {
                if let Some(name) = find_conflict(&String::from_utf8_lossy(&out.stderr)) {
                    let snap = self.refs().unwrap_or_default();
                    let map: HashMap<String, Ref> =
                        snap.refs.into_iter().map(|r| (r.name.clone(), r)).collect();
                    let actual = map.get(&name).map(|r| r.oid.clone()).unwrap_or_default();
                    let expected = txn
                        .updates
                        .iter()
                        .find(|u| u.name == name)
                        .map(|u| u.old_oid.clone())
                        .unwrap_or_default();
                    return Err(GitError::RefConflict {
                        name: name.clone(),
                        expected,
                        actual,
                    });
                }
                return Err(GitError::Subprocess {
                    cmd: "git update-ref --stdin".into(),
                    status: out.status.code(),
                    stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                });
            }
        }

        // Apply symbolic ref updates by writing the HEAD file directly.
        for u in &symref_updates {
            let head_path = self.inner.path.join("HEAD");
            std::fs::write(&head_path, format!("ref: {}\n", u.new_symbolic_target))
                .map_err(GitError::Io)?;
        }
        // Patch the snapshot we started from instead of throwing it away: re-parsing
        // packed-refs + peeling 100 k tags was the O(refs) term every push handed to the next
        // request (~700 ms debug / ~200 ms release at 500 k refs, AGENTS §1.4). `refresh_refs()`
        // bumps the generation without remapping pack indexes.
        self.refresh_refs()?;
        self.refs_changed();
        if let Some(mut c) = before {
            c.pending.push(txn.clone());
            c.key = refs_key(
                &self.inner.path,
                self.inner
                    .refs_gen
                    .load(std::sync::atomic::Ordering::Acquire),
            );
            *self.inner.refs_cache.lock() = Some(c);
        }
        Ok(())
    }

    /// `base` (name-sorted) with `txns` applied in order: O(k log n) lookups + one O(n) copy of
    /// the vector (no parsing, no object reads except peeling a new annotated tag whose update
    /// did not carry `new_peeled`).
    fn patch_snapshot(
        &self,
        base: &RefSnapshotData,
        txns: &[gitcask_proto::v1::RefTransaction],
    ) -> RefSnapshotData {
        let mut refs = base.refs.clone();
        let mut head_target = base.head_target.clone();
        let mut repo: Option<gix::Repository> = None;
        for u in txns.iter().flat_map(|t| t.updates.iter()) {
            if !u.new_symbolic_target.is_empty() {
                if u.name == "HEAD" {
                    head_target = u.new_symbolic_target.clone();
                }
                continue;
            }
            let delete = u.new_oid.is_empty() || u.new_oid.chars().all(|c| c == '0');
            let pos = refs.binary_search_by(|r| r.name.as_str().cmp(u.name.as_str()));
            match (pos, delete) {
                (Ok(i), true) => {
                    refs.remove(i);
                }
                (Err(_), true) => {}
                (pos, false) => {
                    let mut peeled = u.new_peeled.clone();
                    if peeled.is_empty() && u.name.starts_with("refs/tags/") {
                        let r = repo.get_or_insert_with(|| {
                            gix::Repository::from(
                                &gix::ThreadSafeRepository::open(&self.inner.path)
                                    .expect("repo open"),
                            )
                        });
                        if let Ok(oid) = gix_hash::ObjectId::from_hex(u.new_oid.as_bytes()) {
                            peeled = peel_tag(r, oid)
                                .map(|p| p.to_hex().to_string())
                                .unwrap_or_default();
                        }
                    }
                    let entry = Ref {
                        name: u.name.clone(),
                        oid: u.new_oid.clone(),
                        peeled,
                    };
                    match pos {
                        Ok(i) => refs[i] = entry,
                        Err(i) => refs.insert(i, entry),
                    }
                }
            }
        }
        RefSnapshotData { refs, head_target }
    }

    /// Replace ALL refs + HEAD by writing `packed-refs` directly and removing
    /// loose refs. Fast for very large ref sets.
    pub fn load_ref_snapshot(&self, snap: &gitcask_proto::v1::RefSnapshot) -> Result<(), GitError> {
        self.refs_changed();
        let path = &self.inner.path;
        let packed = path.join("packed-refs");
        let mut content = String::new();
        content.push_str("# pack-refs with: peeled fully-peeled sorted \n");
        let mut refs = snap.refs.clone();
        refs.sort_by(|a, b| a.name.cmp(&b.name));
        for r in &refs {
            content.push_str(&format!("{} {}\n", r.oid, r.name));
            if !r.peeled.is_empty() {
                content.push_str(&format!("^{}\n", r.peeled));
            }
        }
        // Atomic write.
        let tmp = packed.with_extension("tmp");
        std::fs::write(&tmp, content).map_err(GitError::Io)?;
        rename_atomic(&tmp, &packed)?;

        // Remove loose refs (everything under refs/, keep HEAD).
        let refs_dir = path.join("refs");
        if refs_dir.exists() {
            let _ = std::fs::remove_dir_all(&refs_dir);
            // gix requires the refs directory to exist; recreate the standard
            // skeleton so the repository remains openable.
            let _ = std::fs::create_dir_all(refs_dir.join("heads"));
            let _ = std::fs::create_dir_all(refs_dir.join("tags"));
        }
        // Rewrite HEAD symbolic target.
        if !snap.head_target.is_empty() {
            std::fs::write(path.join("HEAD"), format!("ref: {}\n", snap.head_target))
                .map_err(GitError::Io)?;
        }
        self.refresh_refs()?;
        Ok(())
    }

    /// Apply already-committed WAL ref transactions without `git update-ref`.
    ///
    /// `git update-ref` refuses to point a ref at an object that is not in the
    /// local odb, so it cannot be used by a replica that has applied the WAL's
    /// *refs* but not (yet) downloaded its packs ("refs-first" sync, the cheap
    /// cold-start path). The log is trusted (the publisher verified old values
    /// and connectivity), so this merges the updates into the current ref set
    /// in memory and rewrites `packed-refs` + `HEAD` once, exactly like
    /// `load_ref_snapshot`. Peeled values for new annotated tags are filled in
    /// when the tag object happens to be present locally.
    pub fn apply_ref_txns_offline(
        &self,
        txns: &[&gitcask_proto::v1::RefTransaction],
    ) -> Result<(), GitError> {
        let span = tracing::info_span!(
            "git.apply_ref_txns_offline",
            repo = %self.inner.id,
            n_txns = txns.len(),
        );
        let _enter = span.enter();
        let snap = self.refs()?;
        let mut head_target = snap.head_target;
        let mut map: BTreeMap<String, Ref> =
            snap.refs.into_iter().map(|r| (r.name.clone(), r)).collect();
        let repo = self.gix();
        for txn in txns {
            for u in &txn.updates {
                validate_ref_update(u)?;
                if !u.new_symbolic_target.is_empty() {
                    if u.name == "HEAD" {
                        head_target = u.new_symbolic_target.clone();
                    }
                    continue;
                }
                let new_zero = u.new_oid.is_empty() || u.new_oid.chars().all(|c| c == '0');
                if new_zero {
                    map.remove(&u.name);
                    continue;
                }
                let peeled = if !u.new_peeled.is_empty() {
                    u.new_peeled.clone()
                } else if u.name.starts_with("refs/tags/") {
                    gix_hash::ObjectId::from_hex(u.new_oid.as_bytes())
                        .ok()
                        .and_then(|oid| peel_tag(&repo, oid))
                        .map(|p| p.to_hex().to_string())
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                map.insert(
                    u.name.clone(),
                    Ref {
                        name: u.name.clone(),
                        oid: u.new_oid.clone(),
                        peeled,
                    },
                );
            }
        }
        let data = RefSnapshotData {
            refs: map.into_values().collect(),
            head_target,
        };
        self.load_ref_snapshot(&data.into())
    }

    pub fn pack_refs(&self) -> Result<(), GitError> {
        self.git_cmd_sync(&["pack-refs", "--all", "--prune"])?;
        self.refresh_refs()?;
        Ok(())
    }
}

fn find_conflict(stderr: &str) -> Option<String> {
    // git update-ref prints: "cannot lock ref '<name>' ... : ..." or similar.
    // Best-effort: extract a ref name appearing in a quoted context.
    for line in stderr.lines() {
        if let Some((_, rest)) = line.split_once("cannot lock ref '") {
            if let Some((name, _)) = rest.split_once('\'') {
                return Some(name.to_string());
            }
        }
        if let Some((_, rest)) = line.split_once("ref ") {
            // "ref refs/heads/main: expected ..."
            let name = rest.split([':', ' ', ',']).next().unwrap_or("").trim();
            if name.starts_with("refs/") {
                return Some(name.to_string());
            }
        }
    }
    None
}

pub(crate) fn read_refs(repo_path: &Path) -> Result<RefSnapshotData, GitError> {
    // HEAD symbolic target.
    let head_target = match std::fs::read_to_string(repo_path.join("HEAD")) {
        Ok(s) => {
            let s = s.trim();
            if let Some(t) = s.strip_prefix("ref: ") {
                t.trim().to_string()
            } else {
                // Detached HEAD: not symbolic.
                String::new()
            }
        }
        Err(_) => String::new(),
    };

    let mut map: BTreeMap<String, (String, String)> = BTreeMap::new();

    // packed-refs
    let packed_path = repo_path.join("packed-refs");
    if let Ok(content) = std::fs::read_to_string(&packed_path) {
        let mut last: Option<String> = None;
        for line in content.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(rest) = line.strip_prefix('^') {
                if let Some(name) = &last {
                    if let Some((_, peeled)) = map.get_mut(name) {
                        *peeled = rest.trim().to_string();
                    }
                }
                continue;
            }
            let mut parts = line.splitn(2, ' ');
            let oid = parts.next().unwrap_or("").trim().to_string();
            let name = parts.next().unwrap_or("").trim().to_string();
            if !name.is_empty() {
                map.insert(name.clone(), (oid, String::new()));
                last = Some(name);
            }
        }
    }

    // Loose refs (override packed).
    let refs_dir = repo_path.join("refs");
    walk_loose_refs(&refs_dir, "refs", &mut map);

    // Peel annotated tags that have no packed peel line. Branch refs cannot be
    // tags, so skipping their object lookup keeps mirror pushes linear in the
    // number of distinct tag object IDs rather than all branch refs.
    let refs: Vec<Ref> = map
        .into_iter()
        .map(|(name, (oid, peeled))| Ref { name, oid, peeled })
        .collect();

    let mut data = RefSnapshotData { refs, head_target };
    // Only tags can be annotated. Avoid an object lookup for every branch:
    // mirror pushes routinely contain tens of thousands of branch refs, often
    // all pointing at the same commit.
    if let Ok(tsr) = gix::ThreadSafeRepository::open(repo_path) {
        let repo = gix::Repository::from(&tsr);
        let mut peeled_by_oid: HashMap<String, Option<String>> = HashMap::new();
        for r in &mut data.refs {
            if !r.peeled.is_empty() || !r.name.starts_with("refs/tags/") {
                continue;
            }
            let Some(oid) = gix_hash::ObjectId::from_hex(r.oid.as_bytes()).ok() else {
                continue;
            };
            let peeled = peeled_by_oid
                .entry(r.oid.clone())
                .or_insert_with(|| peel_tag(&repo, oid).map(|p| p.to_hex().to_string()));
            if let Some(peeled) = peeled {
                r.peeled.clone_from(peeled);
            }
        }
    }
    Ok(data)
}

fn walk_loose_refs(dir: &Path, prefix: &str, map: &mut BTreeMap<String, (String, String)>) {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for ent in rd.flatten() {
        let path = ent.path();
        let name = format!("{prefix}/{}", ent.file_name().to_string_lossy());
        if path.is_dir() {
            walk_loose_refs(&path, &name, map);
        } else if path.is_file() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                let s = content.trim();
                if let Some(t) = s.strip_prefix("ref: ") {
                    // Symbolic loose ref: resolve target oid later if present.
                    // We record the target name in the oid slot is wrong;
                    // instead skip (packed-refs usually has the real value, or
                    // the symref target is resolved at read time elsewhere).
                    // For HEAD-only symref we handle separately; loose symrefs
                    // under refs/ are rare. Record empty oid if unresolved.
                    let target = t.trim();
                    if let Some((o, _)) = map.get(target).cloned() {
                        map.insert(name, (o, String::new()));
                    }
                    continue;
                }
                if !s.is_empty() {
                    map.insert(name, (s.to_string(), String::new()));
                }
            }
        }
    }
}

impl LocalRepo {
    /// Record the peeled target of every `refs/tags/*` update in `txn`
    /// (`new_peeled`) so replicas can advertise annotated tags from the WAL
    /// alone. Call on the writer after the pack is installed.
    pub fn fill_peeled(&self, txn: &mut gitcask_proto::v1::RefTransaction) {
        let repo = self.gix();
        for u in &mut txn.updates {
            if !u.name.starts_with("refs/tags/") || u.new_oid.is_empty() || !u.new_peeled.is_empty()
            {
                continue;
            }
            if u.new_oid.bytes().all(|b| b == b'0') {
                continue;
            }
            if let Ok(oid) = gix_hash::ObjectId::from_hex(u.new_oid.as_bytes()) {
                if let Some(p) = peel_tag(&repo, oid) {
                    if p != oid {
                        u.new_peeled = p.to_hex().to_string();
                    }
                }
            }
        }
    }
}

fn peel_tag(repo: &gix::Repository, oid: gix_hash::ObjectId) -> Option<gix_hash::ObjectId> {
    let kind = repo.object_hash();
    let mut cur = oid;
    for _ in 0..16 {
        let obj = match repo.find_object(cur) {
            Ok(o) => o,
            Err(_) => return None,
        };
        if obj.kind == gix_object::Kind::Tag {
            let tag = gix_object::TagRef::from_bytes(&obj.data, kind).ok()?;
            cur = tag.target();
        } else {
            return Some(cur);
        }
    }
    None
}
