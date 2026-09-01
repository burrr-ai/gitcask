//! Simulation tests: safety mode → liveness mode (after TigerBeetle's VOPR,
//! "Simulation Testing For Liveness", 2023).
//!
//! A *cluster* is N gitcask instances (one `Registry` + cache dir each) that
//! share one in-memory bucket, each through its own fault-injecting link
//! (`gitcask_store::fault::FaultStore`). Safety mode rolls the dice on every
//! store op of every link while pushers hammer the WAL. Liveness mode then
//! picks a **core** of instances, heals their links, **freezes** every other
//! link in whatever broken state it is in (black hole, stale-forever, always
//! 412, crashed-mid-CAS, lease holder gone) and demands that the core still
//! converges within a bound:
//!
//! * a push on a core instance is acknowledged,
//! * every core instance syncs to the same head and the same refs,
//! * compaction and checkpoints complete on the core,
//! * a brand-new instance cold-starts from the bucket and sees everything,
//! * and the **truth** (the bucket itself) stays consistent: every ACK'd push
//!   is in the log at its seq with its txn; every pack/segment/checkpoint the
//!   manifest references exists.
//!
//! The bucket is never touched by faults directly (faults live on links), so
//! the truth store is the oracle. Seeds: `GITCASK_SIM_SEED` (one run) or
//! `GITCASK_SIM_SEEDS` (count, default 2). Size: `GITCASK_SIM_PUSHES` per pusher.
//! Failing runs print the link traces and the seed.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use gitcask_git::{IngestOptions, ObjectFormat, RepoId};
use gitcask_proto::v1::{EntryKind, Manifest, RefTransaction, RefUpdate};
use gitcask_store::fault::{FaultPlan, FaultStore};
use gitcask_store::memory::MemoryStore;
use gitcask_store::{DynStore, ObjectStoreExt};
use gitcask_wal::{Registry, RepoHandle};
use prost::Message;

// ---------------------------------------------------------------------------
// Git work repo (a pusher's clone)
// ---------------------------------------------------------------------------

struct WorkRepo {
    dir: tempfile::TempDir,
}

impl WorkRepo {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.name", "Sim"],
            vec!["config", "user.email", "sim@test"],
            vec!["config", "commit.gpgsign", "false"],
        ] {
            let o = Command::new("git")
                .args(&args)
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(
                o.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&o.stderr)
            );
        }
        WorkRepo { dir }
    }
    fn path(&self) -> &Path {
        self.dir.path()
    }
    fn commit(&self, n: u64, salt: &str) -> String {
        std::fs::write(
            self.path().join(format!("f{}.txt", n % 7)),
            format!("{salt}-{n}\n"),
        )
        .unwrap();
        for args in [
            vec!["add", "."],
            vec![
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                &format!("{salt} {n}"),
            ],
        ] {
            let o = Command::new("git")
                .args(&args)
                .current_dir(self.path())
                .output()
                .unwrap();
            assert!(
                o.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&o.stderr)
            );
        }
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(self.path())
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }
    /// Self-contained pack with everything reachable from `head` minus `base`.
    fn pack(&self, head: &str, base: Option<&str>) -> Vec<u8> {
        let mut revs = format!("{head}\n");
        if let Some(b) = base {
            revs.push_str(&format!("^{b}\n"));
        }
        let mut child = Command::new("git")
            .args(["pack-objects", "--stdout", "--revs", "-q"])
            .current_dir(self.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(revs.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "pack-objects: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }
}

// ---------------------------------------------------------------------------
// Cluster
// ---------------------------------------------------------------------------

fn sim_config(cache_dir: &Path) -> gitcask_config::Config {
    let mut cfg = gitcask_config::Config::default();
    cfg.cache.dir = cache_dir.to_path_buf();
    cfg.wal.batch_window = Duration::from_millis(5);
    cfg.wal.freshness_ttl = Duration::ZERO;
    cfg.wal.fsck_objects = false;
    cfg.wal.check_connectivity = false;
    cfg.wal.snapshot_every_entries = 0;
    cfg.wal.checkpoint_interval = Duration::ZERO;
    cfg.wal.checkpoint_tail_bytes = gitcask_config::ByteSize::b(0);
    cfg.compaction.lease_ttl = Duration::from_secs(2);
    cfg.compaction.trigger_packs = 4;
    // The simulator exercises WAL publication, not derived-index CPU work.
    // History-pack/commit-graph builders can dominate tiny zero-latency store
    // runs and obscure the liveness bound without injecting another fault.
    cfg.git.commit_graph = false;
    cfg.store.bucket = "sim".to_string();
    cfg
}

struct Instance {
    name: String,
    link: Arc<FaultStore>,
    registry: Arc<Registry>,
    cfg: Arc<gitcask_config::Config>,
    _cache: tempfile::TempDir,
}

impl Instance {
    fn new(
        truth: &DynStore,
        name: &str,
        seed: u64,
        tweak: &dyn Fn(&mut gitcask_config::Config),
    ) -> Self {
        Self::new_at(truth, name, seed, tempfile::tempdir().unwrap(), tweak)
    }
    /// Same, on an existing cache dir (a restart that keeps "disk": the SSD host's /data survives the container).
    fn new_at(
        truth: &DynStore,
        name: &str,
        seed: u64,
        cache: tempfile::TempDir,
        tweak: &dyn Fn(&mut gitcask_config::Config),
    ) -> Self {
        let mut cfg = sim_config(cache.path());
        tweak(&mut cfg);
        let cfg = Arc::new(cfg);
        let link = FaultStore::new(truth.clone(), name, seed);
        link.set_trace(true);
        let registry = Registry::new(link.clone() as DynStore, cfg.clone());
        Instance {
            name: name.to_string(),
            link,
            registry,
            cfg,
            _cache: cache,
        }
    }
    async fn open(&self, id: &RepoId) -> Result<Arc<RepoHandle>> {
        Ok(self.registry.open(id).await?)
    }
}

struct Cluster {
    #[allow(dead_code)]
    seed: u64,
    truth: DynStore,
    id: RepoId,
    instances: Vec<Instance>,
    next_link_seed: AtomicU64,
}

impl Cluster {
    async fn new(seed: u64, n: usize) -> Result<Self> {
        let truth: DynStore = MemoryStore::shared();
        let id = RepoId::new("sim", &format!("r{seed}"))?;
        let mut c = Cluster {
            seed,
            truth,
            id,
            instances: Vec::new(),
            next_link_seed: AtomicU64::new(seed * 1000),
        };
        for i in 0..n {
            c.add_instance(&format!("i{i}"), &|_| {});
        }
        // Create through a healthy link.
        c.instances[0]
            .registry
            .create(&c.id, ObjectFormat::Sha1)
            .await?;
        Ok(c)
    }
    fn add_instance(&mut self, name: &str, tweak: &dyn Fn(&mut gitcask_config::Config)) -> usize {
        let s = self.next_link_seed.fetch_add(1, Ordering::Relaxed);
        self.instances
            .push(Instance::new(&self.truth, name, s, tweak));
        self.instances.len() - 1
    }
    /// "Crash" an instance: drop its registry/cache, bring a fresh one up on a
    /// fresh link with the same name.
    fn restart(&mut self, i: usize) {
        let name = self.instances[i].name.clone();
        let s = self.next_link_seed.fetch_add(1, Ordering::Relaxed);
        let fresh = Instance::new(&self.truth, &name, s, &|_| {});
        let old = std::mem::replace(&mut self.instances[i], fresh);
        drop(old);
    }
    /// A throwaway healthy observer (fresh cache, no faults) for oracles.
    fn observer(&self) -> Instance {
        Instance::new(&self.truth, "observer", 0, &|_| {})
    }
    fn repo_prefix(&self) -> String {
        format!("repos/{}/{}/", self.id.owner(), self.id.name())
    }
    async fn truth_manifest(&self) -> Result<Manifest> {
        let key = format!("{}manifest.pb", self.repo_prefix());
        let (_, b) = self
            .truth
            .get_bytes(&key)
            .await?
            .ok_or_else(|| anyhow!("manifest missing in truth"))?;
        Ok(Manifest::decode(b)?)
    }
    fn dump_traces(&self) -> String {
        let mut s = String::new();
        for i in &self.instances {
            s.push_str(&format!(
                "--- link {} ({})\n",
                i.name,
                i.link.stats().summary()
            ));
            for l in i
                .link
                .take_trace()
                .iter()
                .rev()
                .take(40)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
            {
                s.push_str(l);
                s.push('\n');
            }
        }
        s
    }
}

// ---------------------------------------------------------------------------
// Pushers
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Acked {
    seq: u64,
    refname: String,
    old: String,
    new: String,
}

struct Pusher {
    idx: usize,
    work: WorkRepo,
    refname: String,
    n: u64,
    tip: String,
    acked: Vec<Acked>,
    errors: Vec<String>,
    rejected: u64,
}

impl Pusher {
    fn new(idx: usize) -> Self {
        Pusher {
            idx,
            work: WorkRepo::new(),
            refname: format!("refs/heads/p{idx}"),
            n: 0,
            tip: String::new(),
            acked: Vec::new(),
            errors: Vec::new(),
            rejected: 0,
        }
    }

    /// One push of one new commit through `inst`. Returns Ok(true) when
    /// acknowledged, Ok(false) when rejected/errored (recorded), Err only for
    /// harness bugs.
    async fn push_once(&mut self, inst: &Instance, id: &RepoId, timeout: Duration) -> Result<bool> {
        self.n += 1;
        let new = self.work.commit(self.n, &format!("p{}", self.idx));
        let handle = match tokio::time::timeout(timeout, inst.open(id)).await {
            Ok(Ok(h)) => h,
            Ok(Err(e)) => {
                self.errors.push(format!("open: {e}"));
                self.n -= 1;
                return Ok(false);
            }
            Err(_) => {
                self.errors.push("open: timeout".into());
                self.n -= 1;
                return Ok(false);
            }
        };
        // What does this instance believe our ref is? (refs-level sync)
        let current = match tokio::time::timeout(timeout, handle.sync_refs()).await {
            Ok(Ok(g)) => {
                drop(g);
                handle
                    .local()
                    .refs()
                    .ok()
                    .and_then(|s| {
                        s.refs
                            .into_iter()
                            .find(|r| r.name == self.refname)
                            .map(|r| r.oid)
                    })
                    .unwrap_or_default()
            }
            Ok(Err(e)) => {
                self.errors.push(format!("sync_refs: {e}"));
                self.n -= 1;
                return Ok(false);
            }
            Err(_) => {
                self.errors.push("sync_refs: timeout".into());
                self.n -= 1;
                return Ok(false);
            }
        };
        let base = if current.is_empty() {
            None
        } else {
            Some(current.as_str())
        };
        let pack = self.work.pack(&new, base);
        let ingested = match tokio::time::timeout(
            timeout,
            handle.local().ingest_pack(
                std::io::Cursor::new(pack),
                IngestOptions {
                    fsck: false,
                    max_bytes: None,
                    thin: false,
                },
            ),
        )
        .await
        {
            Ok(Ok(Some(p))) => Some(p),
            Ok(Ok(None)) => None,
            Ok(Err(e)) => {
                self.errors.push(format!("ingest: {e}"));
                self.n -= 1;
                return Ok(false);
            }
            Err(_) => {
                self.errors.push("ingest: timeout".into());
                self.n -= 1;
                return Ok(false);
            }
        };
        let txn = RefTransaction {
            updates: vec![RefUpdate {
                name: self.refname.clone(),
                old_oid: current.clone(),
                new_oid: new.clone(),
                new_symbolic_target: String::new(),
                new_peeled: String::new(),
            }],
            push_options: vec![],
            atomic: true,
        };
        // We performed the request freshness sync above, exactly like
        // receive-pack. Reuse it: publish_push() would add a simulator-only
        // second conditional manifest GET to every healthy push.
        match tokio::time::timeout(
            timeout,
            handle.publish_push_synced(ingested, txn, HashMap::new()),
        )
        .await
        {
            Ok(Ok(res)) if res.per_ref.iter().all(|(_, r)| r.is_ok()) => {
                self.acked.push(Acked {
                    seq: res.seq,
                    refname: self.refname.clone(),
                    old: current,
                    new: new.clone(),
                });
                self.tip = new;
                Ok(true)
            }
            Ok(Ok(res)) => {
                self.rejected += 1;
                self.errors.push(format!("rejected: {:?}", res.per_ref));
                Ok(false)
            }
            Ok(Err(e)) => {
                self.errors.push(format!("publish: {e}"));
                Ok(false)
            }
            Err(_) => {
                self.errors.push("publish: timeout".into());
                Ok(false)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Oracles
// ---------------------------------------------------------------------------

/// Truth-level safety: log is dense and consistent with every ACK; every
/// object the manifest references exists in the bucket.
async fn check_truth(c: &Cluster, pushers: &[Pusher]) -> Result<()> {
    let manifest = c.truth_manifest().await?;
    let obs = c.observer();
    let handle = obs.open(&c.id).await?;
    // The checkpoint (if any) folds the log prefix: refs from its RefSnapshot,
    // entries after it from the tail. Both must exist in the bucket.
    let prefix = c.repo_prefix();
    let cp_seq = manifest.checkpoint.as_ref().map(|cp| cp.seq).unwrap_or(0);
    let mut folded: HashMap<String, String> = HashMap::new();
    if cp_seq > 0 {
        let key = format!(
            "{prefix}{}",
            gitcask_proto::keys::checkpoint_refs_key(cp_seq)
        );
        let (_, b) = c
            .truth
            .get_bytes(&key)
            .await?
            .ok_or_else(|| anyhow!("checkpoint refs missing: {key}"))?;
        let snap = gitcask_proto::v1::RefSnapshot::decode(b)?;
        for r in snap.refs {
            folded.insert(r.name, r.oid);
        }
    }
    let log = handle
        .read_log(cp_seq + 1, None)
        .await
        .context("observer read_log")?;
    ensure!(
        !log.is_empty() || manifest.head_seq == cp_seq,
        "empty log tail with head_seq {} > checkpoint {cp_seq}",
        manifest.head_seq
    );
    // Strictly increasing (gaps are allowed: a seq is burned when a crashed
    // writer left an orphan segment at the head), reaching head.
    for w in log.windows(2) {
        ensure!(
            w[0].seq < w[1].seq,
            "log not strictly increasing: {} then {}",
            w[0].seq,
            w[1].seq
        );
    }
    ensure!(
        log.first().map(|e| e.seq > cp_seq).unwrap_or(true),
        "log tail starts at {} <= checkpoint {cp_seq}",
        log[0].seq
    );
    ensure!(
        log.last().map(|e| e.seq).unwrap_or(cp_seq) == manifest.head_seq,
        "log tail {} != manifest.head_seq {}",
        log.last().map(|e| e.seq).unwrap_or(cp_seq),
        manifest.head_seq
    );
    // Every ACK after the checkpoint is in the log at its seq with its txn.
    let by_seq: BTreeMap<u64, _> = log.iter().map(|e| (e.seq, e)).collect();
    for p in pushers {
        for a in p.acked.iter().filter(|a| a.seq > cp_seq) {
            let e = by_seq.get(&a.seq).ok_or_else(|| {
                anyhow!("acked seq {} missing from log (pusher {})", a.seq, p.idx)
            })?;
            ensure!(
                e.kind == EntryKind::Push as i32,
                "seq {} is not a PUSH",
                a.seq
            );
            let txn = e
                .txn
                .as_ref()
                .ok_or_else(|| anyhow!("seq {} has no txn", a.seq))?;
            let u = txn
                .updates
                .iter()
                .find(|u| u.name == a.refname)
                .ok_or_else(|| anyhow!("seq {} lacks {}", a.seq, a.refname))?;
            ensure!(
                u.old_oid == a.old && u.new_oid == a.new,
                "seq {} txn {:?} != ack {:?}",
                a.seq,
                u,
                a
            );
        }
    }
    // Folded refs: each pusher's ref at its last ACK'd tip unless a later
    // (errored-but-committed) push moved it further along the same chain.
    for e in &log {
        if let Some(t) = &e.txn {
            for u in &t.updates {
                folded.insert(u.name.clone(), u.new_oid.clone());
            }
        }
    }
    for p in pushers {
        if let Some(last) = p.acked.last() {
            let f = folded.get(&p.refname).cloned().unwrap_or_default();
            ensure!(!f.is_empty(), "ref {} vanished from fold", p.refname);
            // f must be last.new or a commit pushed after it (the ack'd or an
            // errored-but-committed push along the same chain).
            let later = log.iter().filter(|e| e.seq > last.seq).any(|e| {
                e.txn
                    .as_ref()
                    .map(|t| {
                        t.updates
                            .iter()
                            .any(|u| u.name == p.refname && u.new_oid == f)
                    })
                    .unwrap_or(false)
            }) || last.seq <= cp_seq;
            ensure!(
                f == last.new || later,
                "ref {} folded to {f}, last ack {}",
                p.refname,
                last.new
            );
        }
    }
    // Referenced objects exist in truth.
    for pk in &manifest.packs {
        for key in [
            gitcask_proto::keys::pack_key(&pk.checksum),
            gitcask_proto::keys::idx_key(&pk.checksum),
        ] {
            ensure!(
                c.truth.exists(&format!("{prefix}{key}")).await?,
                "manifest pack side-file missing: {key}"
            );
        }
    }
    for seg in &manifest.log_segments {
        ensure!(
            c.truth.exists(&format!("{prefix}{}", seg.key)).await?,
            "manifest log segment missing: {}",
            seg.key
        );
    }
    if let Some(cp) = &manifest.checkpoint {
        for key in [
            gitcask_proto::keys::checkpoint_key(cp.seq),
            gitcask_proto::keys::checkpoint_refs_key(cp.seq),
        ] {
            ensure!(
                c.truth.exists(&format!("{prefix}{key}")).await?,
                "checkpoint object missing: {key}"
            );
        }
    }
    // Every live pack's objects: every folded ref tip is an object the observer
    // can materialize (full sync through a clean link).
    let _g = handle.sync_full().await.context("observer sync_full")?;
    for (name, oid) in &folded {
        let id = gix_hash::ObjectId::from_hex(oid.as_bytes())?;
        ensure!(
            handle.local().has_object(&id),
            "observer lacks tip {oid} of {name} after full sync"
        );
    }
    Ok(())
}

/// Liveness of the core: pushes ACK, syncs converge, maintenance completes,
/// a cold instance comes up. `bound` is the wall-clock budget per step.
async fn check_core_liveness(
    c: &mut Cluster,
    core: &[usize],
    pushers: &mut [Pusher],
    bound: Duration,
) -> Result<()> {
    eprintln!("liveness: step 1 core pushes");
    // 1. A push on every core instance is acknowledged (retrying is allowed: a
    //    core instance may need a moment to resync after its link healed, but
    //    it must get there).
    for (k, &i) in core.iter().enumerate() {
        let p = &mut pushers[k % pushers.len()];
        let t = Instant::now();
        let mut ok = false;
        while t.elapsed() < bound {
            // `push_once` has per-stage timeouts; cap the whole attempt too so
            // retries cannot multiply the liveness bound (sync + ingest +
            // publish) and turn a failing seed into a hung test.
            let remaining = bound.saturating_sub(t.elapsed());
            match tokio::time::timeout(remaining, p.push_once(&c.instances[i], &c.id, remaining))
                .await
            {
                Ok(Ok(true)) => {
                    ok = true;
                    break;
                }
                Ok(Ok(false)) => {}
                Ok(Err(e)) => return Err(e),
                Err(_) => p.errors.push("whole push attempt: timeout".into()),
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        ensure!(
            ok,
            "liveness: push on core instance {} never acknowledged within {bound:?}; last errors: {:?}",
            c.instances[i].name,
            p.errors.iter().rev().take(3).collect::<Vec<_>>()
        );
    }
    eprintln!("liveness: step 2 converge refs");
    // 2. Every core instance syncs to the truth head and agrees on refs.
    let truth = c.truth_manifest().await?;
    let mut views = Vec::new();
    for &i in core {
        let h = c.instances[i].open(&c.id).await?;
        let t = Instant::now();
        loop {
            match tokio::time::timeout(bound, h.sync_refs()).await {
                Ok(Ok(g)) => {
                    drop(g);
                    if h.applied_seq() >= truth.head_seq {
                        break;
                    }
                }
                Ok(Err(e)) => tracing::warn!("core {} sync_refs: {e}", c.instances[i].name),
                Err(_) => bail!(
                    "liveness: core {} sync_refs hung > {bound:?}",
                    c.instances[i].name
                ),
            }
            ensure!(
                t.elapsed() < bound,
                "liveness: core {} stuck at seq {} < truth {}",
                c.instances[i].name,
                h.applied_seq(),
                truth.head_seq
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let refs: BTreeMap<String, String> = h
            .local()
            .refs()?
            .refs
            .into_iter()
            .map(|r| (r.name, r.oid))
            .collect();
        views.push((c.instances[i].name.clone(), refs));
    }
    for w in views.windows(2) {
        ensure!(
            w[0].1 == w[1].1,
            "liveness: core refs diverge between {} and {}",
            w[0].0,
            w[1].0
        );
    }
    eprintln!("liveness: step 3 checkpoint + compaction");
    // 3. Maintenance on the core: checkpoint + forced compaction.
    let h = c.instances[core[0]].open(&c.id).await?;
    tokio::time::timeout(bound, h.write_checkpoint())
        .await
        .map_err(|_| anyhow!("liveness: checkpoint hung"))??;
    eprintln!("liveness: checkpoint complete; starting compaction");
    let t = Instant::now();
    loop {
        let out = tokio::time::timeout(
            bound,
            gitcask_server::ops::compact_repo(
                &h,
                &c.instances[core[0]].cfg,
                gitcask_server::ops::CompactRequest { force: true },
                &gitcask_server::ops::noop_log,
            ),
        )
        .await
        .map_err(|_| anyhow!("liveness: compaction hung > {bound:?}"))?;
        match out {
            Ok(gitcask_server::ops::CompactOutcome::Published { .. })
            | Ok(gitcask_server::ops::CompactOutcome::NotTriggered { .. }) => break,
            Ok(gitcask_server::ops::CompactOutcome::LeaseHeld) => {
                ensure!(
                    t.elapsed() < bound,
                    "liveness: compaction lease never became available within {bound:?}"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => {
                ensure!(
                    t.elapsed() < bound,
                    "liveness: compaction kept failing within {bound:?}: {e}"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
    eprintln!("liveness: step 4 cold start");
    // 4. A cold instance comes up from the bucket alone.
    let cold = c.add_instance("cold", &|_| {});
    let h = c.instances[cold].open(&c.id).await?;
    tokio::time::timeout(bound, h.sync_full())
        .await
        .map_err(|_| anyhow!("liveness: cold sync_full hung"))??;
    let truth = c.truth_manifest().await?;
    ensure!(
        h.applied_seq() == truth.head_seq,
        "cold instance at {} != truth {}",
        h.applied_seq(),
        truth.head_seq
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

fn seeds() -> Vec<u64> {
    if let Ok(s) = std::env::var("GITCASK_SIM_SEED") {
        return vec![s.parse().expect("GITCASK_SIM_SEED")];
    }
    let n: u64 = std::env::var("GITCASK_SIM_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    (1..=n).map(|i| 0xC0FFEE + i * 7919).collect()
}
fn pushes_per_pusher() -> u64 {
    std::env::var("GITCASK_SIM_PUSHES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(12)
}

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn chance(&mut self, p: f64) -> bool {
        (self.next() as f64 / (1u64 << 31) as f64) < p
    }
}

/// The general run: chaos on every link while P pushers hammer N instances
/// (with crash/restarts), then liveness with a random core.
async fn run_safety_then_liveness(seed: u64) -> Result<()> {
    let n_instances = 4;
    let n_pushers = 3;
    let mut c = Cluster::new(seed, n_instances).await?;
    let mut rng = Lcg(seed);
    let mut pushers: Vec<Pusher> = (0..n_pushers).map(Pusher::new).collect();

    // Safety mode: chaos everywhere (moderate so that pushes still land).
    for inst in &c.instances {
        inst.link.set(FaultPlan::chaos(0.04));
    }
    let per = pushes_per_pusher();
    let op_timeout = Duration::from_secs(10);
    for round in 0..per {
        for p in pushers.iter_mut() {
            let i = rng.below(n_instances as u64) as usize;
            let _ = p.push_once(&c.instances[i], &c.id, op_timeout).await?;
        }
        // Random crash: replace an instance (its in-flight state is gone).
        if rng.chance(0.2) {
            let i = rng.below(n_instances as u64) as usize;
            c.restart(i);
            c.instances[i].link.set(FaultPlan::chaos(0.04));
        }
        // Occasionally somebody checkpoints or compacts under chaos.
        if round % 4 == 3 {
            let i = rng.below(n_instances as u64) as usize;
            if let Ok(h) = c.instances[i].open(&c.id).await {
                let _ = tokio::time::timeout(op_timeout, h.write_checkpoint()).await;
                let cfg = c.instances[i].cfg.clone();
                let _ = tokio::time::timeout(
                    op_timeout,
                    gitcask_server::ops::compact_repo(
                        &h,
                        &cfg,
                        gitcask_server::ops::CompactRequest {
                            force: rng.chance(0.5),
                        },
                        &gitcask_server::ops::noop_log,
                    ),
                )
                .await;
            }
        }
    }
    let acked: usize = pushers.iter().map(|p| p.acked.len()).sum();
    let errs: usize = pushers.iter().map(|p| p.errors.len()).sum();
    eprintln!("[seed {seed}] safety phase: {acked} acked, {errs} errored/rejected pushes");
    ensure!(acked > 0, "chaos too strong: nothing was ever acknowledged");

    // Truth must be consistent at the end of safety mode already.
    if let Err(e) = check_truth(&c, &pushers).await {
        eprintln!("{}", c.dump_traces());
        return Err(e.context("truth after safety mode"));
    }

    // Liveness mode: pick a core of 2, heal it, freeze the rest in nasty states.
    let mut idx: Vec<usize> = (0..n_instances).collect();
    for k in (1..idx.len()).rev() {
        let j = rng.below(k as u64 + 1) as usize;
        idx.swap(k, j);
    }
    let core = &idx[..2];
    for &i in core {
        c.instances[i].link.heal();
    }
    let link_delay = Some((Duration::from_millis(1), Duration::from_millis(2)));
    let frozen: Vec<FaultPlan> = vec![
        FaultPlan::black_hole(),
        FaultPlan {
            p_stale_304: 1.0,
            delay: link_delay,
            ..Default::default()
        },
        FaultPlan {
            p_cas_fail: 1.0,
            delay: link_delay,
            ..Default::default()
        },
        FaultPlan {
            p_err_after: 1.0,
            delay: link_delay,
            ..Default::default()
        },
    ];
    for (k, &i) in idx[2..].iter().enumerate() {
        c.instances[i]
            .link
            .set(frozen[(k + rng.below(4) as usize) % frozen.len()].clone());
    }
    // Non-core pushers keep hammering the frozen links in the background (they
    // may never interfere with the core).
    let bg: Vec<_> = idx[2..]
        .iter()
        .map(|&i| {
            let link = c.instances[i].link.clone();
            let reg = c.instances[i].registry.clone();
            let id = c.id.clone();
            tokio::spawn(async move {
                let mut p = Pusher::new(90 + i);
                let inst = Instance {
                    name: "bg".into(),
                    link,
                    registry: reg,
                    cfg: Arc::new(sim_config(Path::new("/nonexistent"))),
                    _cache: tempfile::tempdir().unwrap(),
                };
                for _ in 0..20 {
                    let _ = p.push_once(&inst, &id, Duration::from_millis(500)).await;
                    // MemoryStore completes operations without network latency.
                    // Yield between requests so a stale link models a busy
                    // client, not an impossible zero-latency CPU denial loop
                    // that monopolizes a Tokio worker and the truth mutex.
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
        })
        .collect();

    let bound = Duration::from_secs(20);
    let core_names: Vec<_> = core.iter().map(|&i| c.instances[i].name.clone()).collect();
    eprintln!("[seed {seed}] liveness phase: core = {core_names:?}");
    let res = check_core_liveness(&mut c, core, &mut pushers, bound).await;
    for b in bg {
        b.abort();
    }
    res.with_context(|| format!("liveness with core {core_names:?}"))?;
    eprintln!("[seed {seed}] final truth oracle");
    tokio::time::timeout(bound, check_truth(&c, &pushers))
        .await
        .map_err(|_| anyhow!("truth oracle hung after liveness mode > {bound:?}"))?
        .context("truth after liveness mode")?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sim_safety_then_liveness() {
    for seed in seeds() {
        let t = Instant::now();
        let r = run_safety_then_liveness(seed).await;
        eprintln!(
            "[seed {seed}] {:?} in {:.1}s",
            r.as_ref().map(|_| "ok"),
            t.elapsed().as_secs_f64()
        );
        if let Err(e) = r {
            panic!(
                "seed {seed} failed: {e:#}\nreproduce: GITCASK_SIM_SEED={seed} cargo test -p gitcask-server --test sim sim_safety_then_liveness -- --nocapture"
            );
        }
    }
}

/// Liveness 1: the compaction lease holder dies (crash between acquire and
/// release, no heartbeat). The core must compact once the TTL expires.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn liveness_compaction_after_lease_holder_dies() -> Result<()> {
    let mut c = Cluster::new(11, 2).await?;
    let mut p = Pusher::new(0);
    for _ in 0..5 {
        ensure!(
            p.push_once(&c.instances[0], &c.id, Duration::from_secs(10))
                .await?
        );
    }
    // Instance 1 takes the lease and "crashes" (guard leaked, never released).
    let h1 = c.instances[1].open(&c.id).await?;
    let lease_store: DynStore = Arc::new(h1.store().clone());
    let guard = gitcask_store::coord::try_acquire(
        lease_store,
        &gitcask_proto::keys::lease_key("compact"),
        "dead-instance",
        "compact",
        c.instances[1].cfg.compaction.lease_ttl,
    )
    .await?
    .expect("lease free");
    std::mem::forget(guard);
    c.restart(1);

    let t = Instant::now();
    let h0 = c.instances[0].open(&c.id).await?;
    loop {
        let out = gitcask_server::ops::compact_repo(
            &h0,
            &c.instances[0].cfg,
            gitcask_server::ops::CompactRequest { force: true },
            &gitcask_server::ops::noop_log,
        )
        .await?;
        match out {
            gitcask_server::ops::CompactOutcome::Published { .. } => break,
            gitcask_server::ops::CompactOutcome::LeaseHeld => {
                ensure!(
                    t.elapsed() < Duration::from_secs(15),
                    "lease of a dead holder never expired (ttl {:?})",
                    c.instances[0].cfg.compaction.lease_ttl
                );
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            other => bail!("unexpected {other:?}"),
        }
    }
    eprintln!(
        "compacted {:.1}s after the holder died",
        t.elapsed().as_secs_f64()
    );
    check_truth(&c, std::slice::from_ref(&p)).await?;
    Ok(())
}

/// Liveness 2: a process crash in the middle of a publish (the publisher task
/// panics on the manifest CAS). The instance must keep accepting pushes for
/// that repo afterwards — a dead single-flight publisher must not wedge it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn liveness_publisher_survives_a_crash_mid_publish() -> Result<()> {
    let c = Cluster::new(12, 1).await?;
    let mut p = Pusher::new(0);
    ensure!(
        p.push_once(&c.instances[0], &c.id, Duration::from_secs(10))
            .await?
    );
    c.instances[0].link.set(FaultPlan {
        panic_once_keys: vec!["put:manifest.pb".into()],
        ..Default::default()
    });
    // This push hits the panic inside the publisher task.
    let first = p
        .push_once(&c.instances[0], &c.id, Duration::from_secs(10))
        .await?;
    eprintln!("push during crash acked={first}; errors={:?}", p.errors);
    c.instances[0].link.heal();
    // Now the instance must recover: pushes are acknowledged again.
    let t = Instant::now();
    let mut ok = false;
    while t.elapsed() < Duration::from_secs(10) {
        if p.push_once(&c.instances[0], &c.id, Duration::from_secs(5))
            .await?
        {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    ensure!(
        ok,
        "instance never accepted a push again after its publisher crashed; errors: {:?}\n{}",
        p.errors,
        c.dump_traces()
    );
    check_truth(&c, std::slice::from_ref(&p)).await?;
    Ok(())
}

/// Liveness 3: a stale-forever instance (its conditional GETs always answer
/// 304: it never learns of anyone's writes) pushes in a tight loop against the
/// same repo. The healthy core must keep acknowledging pushes quickly and
/// without errors: a non-core instance may not starve the manifest.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn liveness_stale_instance_cannot_starve_the_core() -> Result<()> {
    let c = Cluster::new(13, 2).await?;
    let mut core_p = Pusher::new(0);
    ensure!(
        core_p
            .push_once(&c.instances[0], &c.id, Duration::from_secs(10))
            .await?
    );
    // Let the stale instance see the repo once, then freeze its view.
    let mut stale_p = Pusher::new(1);
    ensure!(
        stale_p
            .push_once(&c.instances[1], &c.id, Duration::from_secs(10))
            .await?
    );
    c.instances[1].link.set(FaultPlan::stale_forever());

    let stale_link = c.instances[1].link.clone();
    let stale_reg = c.instances[1].registry.clone();
    let id = c.id.clone();
    let bg = tokio::spawn(async move {
        let inst = Instance {
            name: "stale".into(),
            link: stale_link,
            registry: stale_reg,
            cfg: Arc::new(sim_config(Path::new("/nonexistent"))),
            _cache: tempfile::tempdir().unwrap(),
        };
        let mut n = 0u64;
        loop {
            let _ = stale_p.push_once(&inst, &id, Duration::from_secs(2)).await;
            n += 1;
            if n % 10 == 0 {
                tracing::info!(
                    "stale pusher: {n} attempts, last: {:?}",
                    stale_p.errors.last()
                );
            }
        }
    });

    let mut lat = Vec::new();
    let mut fails = Vec::new();
    for _ in 0..25 {
        let t = Instant::now();
        let ok = core_p
            .push_once(&c.instances[0], &c.id, Duration::from_secs(10))
            .await?;
        lat.push(t.elapsed());
        if !ok {
            fails.push(core_p.errors.last().cloned().unwrap_or_default());
        }
    }
    bg.abort();
    lat.sort();
    let p50 = lat[lat.len() / 2];
    let p99 = lat[lat.len() * 99 / 100];
    eprintln!(
        "core pushes next to a stale hammerer: p50 {p50:?} p99 {p99:?}, failures {}",
        fails.len()
    );
    ensure!(
        fails.is_empty(),
        "core pushes failed next to a stale instance: {fails:?}"
    );
    ensure!(
        p99 < Duration::from_secs(5),
        "core push p99 {p99:?} — starved"
    );
    check_truth(&c, &[core_p]).await?;
    Ok(())
}

/// Liveness 4: the bucket ACKs the manifest CAS but the response is lost.
/// The push errors (fine), but the truth must stay consistent (no committed
/// log segment may be deleted as an "orphan") and everybody must still sync.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn liveness_after_a_lost_cas_response() -> Result<()> {
    let mut c = Cluster::new(14, 2).await?;
    let mut p = Pusher::new(0);
    ensure!(
        p.push_once(&c.instances[0], &c.id, Duration::from_secs(10))
            .await?
    );
    c.instances[0].link.set(
        FaultPlan {
            p_err_after: 1.0,
            ..Default::default()
        }
        .with_only(&["manifest.pb"]),
    );
    let acked = p
        .push_once(&c.instances[0], &c.id, Duration::from_secs(10))
        .await?;
    eprintln!(
        "push with lost CAS response: acked={acked}, last error {:?}",
        p.errors.last()
    );
    c.instances[0].link.heal();
    let head = c.truth_manifest().await?.head_seq;
    eprintln!("truth head after lost response: {head}");
    // Everyone, including a cold instance, must be able to sync.
    let r = check_truth(&c, std::slice::from_ref(&p)).await;
    if let Err(e) = &r {
        eprintln!("{}", c.dump_traces());
        bail!("truth inconsistent after a lost CAS response: {e:#}");
    }
    let cold = c.add_instance("cold", &|_| {});
    let h = c.instances[cold].open(&c.id).await?;
    tokio::time::timeout(Duration::from_secs(10), h.sync_full())
        .await
        .map_err(|_| anyhow!("cold sync hung"))??;
    ensure!(
        h.applied_seq() == head,
        "cold at {} != {head}",
        h.applied_seq()
    );
    // And the writer itself keeps working.
    ensure!(
        p.push_once(&c.instances[0], &c.id, Duration::from_secs(10))
            .await?,
        "writer wedged: {:?}",
        p.errors.last()
    );
    check_truth(&c, std::slice::from_ref(&p)).await?;
    Ok(())
}

/// Liveness 7 (found by `sim_safety_then_liveness`): a writer crashes between
/// its log PUT and its manifest CAS, leaving `log/<head+1>.pb` orphaned. Every
/// later writer used to 412 on that key forever ("retry exhausted"): one crash
/// = no more pushes to the repo, from anyone. The core must publish anyway.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn liveness_orphaned_log_segment_does_not_block_writers() -> Result<()> {
    let mut c = Cluster::new(17, 2).await?;
    let mut p = Pusher::new(0);
    ensure!(
        p.push_once(&c.instances[0], &c.id, Duration::from_secs(10))
            .await?
    );
    // Instance 1 crashes right after its log PUT (the manifest CAS never happens).
    c.instances[1].link.set(FaultPlan {
        panic_once_keys: vec!["put:manifest.pb".into()],
        ..Default::default()
    });
    let mut crasher = Pusher::new(1);
    let _ = crasher
        .push_once(&c.instances[1], &c.id, Duration::from_secs(10))
        .await?;
    let head = c.truth_manifest().await?.head_seq;
    let orphan = format!(
        "{}{}",
        c.repo_prefix(),
        gitcask_proto::keys::log_segment_key(head + 1)
    );
    ensure!(
        c.truth.exists(&orphan).await?,
        "setup: expected an orphan at {orphan}"
    );
    c.restart(1);

    // Core: pushes from both instances must land.
    let t = Instant::now();
    for i in 0..2 {
        let mut ok = false;
        while t.elapsed() < Duration::from_secs(20) {
            if p.push_once(&c.instances[i], &c.id, Duration::from_secs(10))
                .await?
            {
                ok = true;
                break;
            }
        }
        ensure!(
            ok,
            "writers blocked by an orphaned log segment: {:?}",
            p.errors.last()
        );
    }
    eprintln!(
        "pushes past the orphan took {:.2}s; errors seen: {:?}",
        t.elapsed().as_secs_f64(),
        p.errors
    );
    // Compaction (its own CAS loop) must get past it too.
    let h = c.instances[0].open(&c.id).await?;
    let out = gitcask_server::ops::compact_repo(
        &h,
        &c.instances[0].cfg,
        gitcask_server::ops::CompactRequest { force: true },
        &gitcask_server::ops::noop_log,
    )
    .await?;
    ensure!(
        matches!(out, gitcask_server::ops::CompactOutcome::Published { .. }),
        "{out:?}"
    );
    // The orphan was swept.
    ensure!(
        !c.truth.exists(&orphan).await?,
        "orphan {orphan} still in the bucket after a commit burned past it"
    );
    check_truth(&c, std::slice::from_ref(&p)).await?;
    Ok(())
}

/// Liveness 5: a cold instance whose pack downloads are truncated for a while.
/// Once its link heals, it must finish syncing — a half-downloaded pack on
/// disk may not poison every later attempt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn liveness_cold_start_through_truncated_pack_reads() -> Result<()> {
    let mut c = Cluster::new(15, 1).await?;
    let mut p = Pusher::new(0);
    for _ in 0..4 {
        ensure!(
            p.push_once(&c.instances[0], &c.id, Duration::from_secs(10))
                .await?
        );
    }
    let cold = c.add_instance("cold", &|_| {});
    c.instances[cold].link.set(
        FaultPlan {
            p_truncate: 1.0,
            ..Default::default()
        }
        .with_only(&[".pack", ".idx"]),
    );
    let h = c.instances[cold].open(&c.id).await?;
    let mut failures = 0;
    for _ in 0..3 {
        match tokio::time::timeout(Duration::from_secs(10), h.sync_full()).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                failures += 1;
                eprintln!("sync_full under truncation: {e}");
            }
            Err(_) => bail!("sync_full hung under truncation"),
        }
    }
    eprintln!("{failures} failed syncs under truncation (expected > 0)");
    c.instances[cold].link.heal();
    let t = Instant::now();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), h.sync_full()).await {
            Ok(Ok(_)) => break,
            Ok(Err(e)) => {
                ensure!(
                    t.elapsed() < Duration::from_secs(10),
                    "healed cold instance never syncs: {e}\n{}",
                    c.dump_traces()
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(_) => bail!("sync_full hung after heal"),
        }
    }
    // Objects really are there.
    for a in &p.acked {
        let id = gix_hash::ObjectId::from_hex(a.new.as_bytes())?;
        ensure!(h.local().has_object(&id), "cold instance lacks {}", a.new);
    }
    // Compaction on the healed instance works on what it downloaded.
    let out = gitcask_server::ops::compact_repo(
        &h,
        &c.instances[cold].cfg,
        gitcask_server::ops::CompactRequest { force: true },
        &gitcask_server::ops::noop_log,
    )
    .await?;
    ensure!(
        matches!(out, gitcask_server::ops::CompactOutcome::Published { .. }),
        "{out:?}"
    );
    check_truth(&c, std::slice::from_ref(&p)).await?;
    Ok(())
}

/// Liveness 6: a black-holed instance holds a read guard / is mid-sync forever;
/// its pushers hang. The rest of the cluster must not notice: pushes on the
/// healthy instance ACK at normal latency, and a checkpoint+compaction go through.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn liveness_black_holed_instance_is_invisible_to_the_core() -> Result<()> {
    let c = Cluster::new(16, 2).await?;
    let mut p0 = Pusher::new(0);
    let mut p1 = Pusher::new(1);
    ensure!(
        p0.push_once(&c.instances[0], &c.id, Duration::from_secs(10))
            .await?
    );
    ensure!(
        p1.push_once(&c.instances[1], &c.id, Duration::from_secs(10))
            .await?
    );
    c.instances[1].link.set(FaultPlan::black_hole());
    let (link, reg, id) = (
        c.instances[1].link.clone(),
        c.instances[1].registry.clone(),
        c.id.clone(),
    );
    let bg = tokio::spawn(async move {
        let inst = Instance {
            name: "hole".into(),
            link,
            registry: reg,
            cfg: Arc::new(sim_config(Path::new("/nonexistent"))),
            _cache: tempfile::tempdir().unwrap(),
        };
        for _ in 0..5 {
            let _ = p1.push_once(&inst, &id, Duration::from_secs(30)).await;
        }
    });
    let mut lat = Vec::new();
    for _ in 0..10 {
        let t = Instant::now();
        ensure!(
            p0.push_once(&c.instances[0], &c.id, Duration::from_secs(10))
                .await?,
            "{:?}",
            p0.errors.last()
        );
        lat.push(t.elapsed());
    }
    lat.sort();
    eprintln!(
        "core push p50 {:?} max {:?} next to a black-holed instance",
        lat[lat.len() / 2],
        lat.last().unwrap()
    );
    let h = c.instances[0].open(&c.id).await?;
    tokio::time::timeout(Duration::from_secs(10), h.write_checkpoint())
        .await
        .map_err(|_| anyhow!("checkpoint hung"))??;
    let out = tokio::time::timeout(
        Duration::from_secs(20),
        gitcask_server::ops::compact_repo(
            &h,
            &c.instances[0].cfg,
            gitcask_server::ops::CompactRequest { force: true },
            &gitcask_server::ops::noop_log,
        ),
    )
    .await
    .map_err(|_| anyhow!("compaction hung"))??;
    ensure!(
        matches!(out, gitcask_server::ops::CompactOutcome::Published { .. }),
        "{out:?}"
    );
    bg.abort();
    check_truth(&c, std::slice::from_ref(&p0)).await?;
    Ok(())
}

/// A request ReadGuard is the pin that promises packs remain on disk. Even a
/// leaked guard must make eviction skip the repo; after it drops, eviction may
/// reclaim the cache.
#[tokio::test]
async fn liveness_leaked_read_guard_pins_cache_until_drop() -> Result<()> {
    let mut c = Cluster::new(19, 1).await?;
    let pinned = c.add_instance("pinned", &|cfg| {
        cfg.cache.evict_idle_after = Duration::ZERO;
    });
    let h = c.instances[pinned].open(&c.id).await?;
    let guard = h.sync_full().await?;
    let path = h.local().path().to_path_buf();

    let report = c.instances[pinned].registry.evict_idle().await?;
    ensure!(
        report.evicted == 0,
        "evicted a repo under an active ReadGuard"
    );
    ensure!(path.exists(), "deleted a pinned repo directory");

    drop(guard);
    let report = c.instances[pinned].registry.evict_idle().await?;
    ensure!(
        report.evicted == 1,
        "repo was not evictable after guard drop"
    );
    ensure!(!path.exists(), "evicted repo directory remains");
    Ok(())
}

/// Checkpoint and compaction both CAS manifest.pb. Whichever wins first, the
/// loser must re-sync and preserve both the checkpoint and compacted pack set.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn liveness_checkpoint_racing_compaction() -> Result<()> {
    let c = Cluster::new(20, 2).await?;
    let mut p = Pusher::new(0);
    for _ in 0..5 {
        ensure!(
            p.push_once(&c.instances[0], &c.id, Duration::from_secs(10))
                .await?
        );
    }
    let h0 = c.instances[0].open(&c.id).await?;
    let h1 = c.instances[1].open(&c.id).await?;
    drop(h1.sync_full().await?);
    let cfg = c.instances[1].cfg.clone();
    let (cp, compact) = tokio::join!(
        h0.write_checkpoint(),
        gitcask_server::ops::compact_repo(
            &h1,
            &cfg,
            gitcask_server::ops::CompactRequest { force: true },
            &gitcask_server::ops::noop_log,
        )
    );
    cp.context("checkpoint lost its CAS race")?;
    ensure!(
        matches!(
            compact?,
            gitcask_server::ops::CompactOutcome::Published { .. }
        ),
        "compaction did not publish"
    );
    check_truth(&c, std::slice::from_ref(&p)).await?;
    Ok(())
}

/// Two maintainers may list the same pending-marker set. Parallel workers on
/// both instances still commit exactly one checkpoint per repository.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_maintainers_safely_consume_the_same_pending_markers() -> Result<()> {
    const REPOS: usize = 30;
    let mut memory = MemoryStore::shared();
    Arc::get_mut(&mut memory).expect("unshared store").latency = Some(Duration::from_millis(1));
    let truth: DynStore = memory;
    let writer_cache = tempfile::tempdir()?;
    let cache0 = tempfile::tempdir()?;
    let cache1 = tempfile::tempdir()?;
    let mut writer_config = sim_config(writer_cache.path());
    writer_config.wal.snapshot_every_entries = 0;
    writer_config.wal.checkpoint_interval = Duration::ZERO;
    writer_config.wal.checkpoint_tail_bytes = gitcask_config::ByteSize::b(0);
    writer_config.maintenance.fsck_interval = Duration::ZERO;
    writer_config.compaction.enabled = false;
    let writer = gitcask_server::AppState::new(Arc::new(writer_config), truth.clone()).await?;
    let make_config = |cache: &Path| {
        let mut cfg = sim_config(cache);
        cfg.wal.snapshot_every_entries = 1;
        cfg.wal.checkpoint_interval = Duration::ZERO;
        cfg.wal.checkpoint_tail_bytes = gitcask_config::ByteSize::b(0);
        cfg.maintenance.workers = 4;
        cfg.maintenance.max_repos_per_pass = REPOS;
        cfg.maintenance.fsck_interval = Duration::ZERO;
        cfg.compaction.enabled = false;
        Arc::new(cfg)
    };
    let mut repos = Vec::with_capacity(REPOS);
    for index in 0..REPOS {
        let id = RepoId::new("sim", &format!("pending-race-{index:02}"))?;
        let handle = writer.registry.create(&id, ObjectFormat::Sha1).await?;
        handle
            .publish_ref_update(
                gitcask_proto::v1::RefTransaction::default(),
                std::collections::HashMap::default(),
            )
            .await?;
        repos.push((id, handle.manifest().revision));
    }
    let state0 = gitcask_server::AppState::new(make_config(cache0.path()), truth.clone()).await?;
    let state1 = gitcask_server::AppState::new(make_config(cache1.path()), truth.clone()).await?;

    let (report0, report1) = tokio::join!(
        gitcask_server::maintain::run_pass(&state0),
        gitcask_server::maintain::run_pass(&state1),
    );
    let report0 = report0?;
    let report1 = report1?;
    ensure!(
        report0.markers == REPOS && report1.markers == REPOS,
        "both maintainers must observe every marker: {report0:?} / {report1:?}"
    );
    // A successful checkpoint operation can be an idempotent observer: after
    // the first maintainer releases the lease, a second maintainer may acquire
    // it using a plan made before the checkpoint and then find the checkpoint
    // already current. Count the durable manifest transition, not task timing.
    for (id, revision_before) in &repos {
        let key = format!(
            "{}{}",
            gitcask_proto::keys::repo_prefix(id.owner(), id.name()),
            gitcask_proto::keys::MANIFEST
        );
        let (_, bytes) = truth
            .get_bytes(&key)
            .await?
            .ok_or_else(|| anyhow!("missing manifest for {id}"))?;
        let manifest = Manifest::decode(bytes)?;
        ensure!(
            manifest.head_seq == 1
                && manifest
                    .checkpoint
                    .as_ref()
                    .is_some_and(|checkpoint| checkpoint.seq == 1),
            "checkpoint must cover {id}'s committed push: {manifest:?}; reports: {report0:?} / {report1:?}"
        );
        ensure!(
            manifest.revision == revision_before.saturating_add(1),
            "exactly one checkpoint manifest CAS for {id}: revision {revision_before} -> {}; reports: {report0:?} / {report1:?}",
            manifest.revision
        );
    }

    // Checkpoint-only work yields once. A second concurrent pass observes idle
    // repositories and conditionally consumes all listed marker versions.
    let (drain0, drain1) = tokio::join!(
        gitcask_server::maintain::run_pass(&state0),
        gitcask_server::maintain::run_pass(&state1),
    );
    drain0?;
    drain1?;
    for (id, _) in repos {
        ensure!(
            truth
                .head(&gitcask_proto::keys::pending_key(id.owner(), id.name()))
                .await?
                .is_none(),
            "completed maintenance should drain {id}"
        );
        let fresh = state1.registry.open(&id).await?;
        drop(fresh.sync_refs().await?);
        ensure!(
            fresh.manifest().checkpoint.as_ref().map(|cp| cp.seq) == Some(1),
            "checkpoint must cover {id}'s committed push"
        );
    }
    Ok(())
}

#[tokio::test]
async fn pending_marker_failure_does_not_fail_a_committed_push() -> Result<()> {
    let c = Cluster::new(21, 1).await?;
    c.instances[0].link.set(
        FaultPlan {
            p_err_before: 1.0,
            ..Default::default()
        }
        .with_only(&[gitcask_proto::keys::PENDING_DIR]),
    );
    let mut pusher = Pusher::new(0);
    ensure!(
        pusher
            .push_once(&c.instances[0], &c.id, Duration::from_secs(10))
            .await?,
        "the committed push must still be acknowledged"
    );
    ensure!(c.truth_manifest().await?.head_seq == 1);
    ensure!(
        c.truth
            .head(&gitcask_proto::keys::pending_key(c.id.owner(), c.id.name()))
            .await?
            .is_none(),
        "the injected marker PUT failure should leave no marker"
    );
    Ok(())
}

/// Exact healthy-link request counts defend the critical-path budgets in
/// `docs/ROUNDTRIPS.md`. `MemoryStore` has no retries, so deltas are deterministic:
/// push = one freshness GET + pack/idx/log PUTs + manifest CAS + pending marker;
/// warm refs = one conditional GET; cold refs = the open's manifest GET + one log tail GET.
#[tokio::test]
async fn healthy_request_round_trip_budgets() -> Result<()> {
    let mut c = Cluster::new(22, 1).await?;
    let mut p = Pusher::new(0);
    let before = c.instances[0].link.stats().ops.load(Ordering::Relaxed);
    ensure!(
        p.push_once(&c.instances[0], &c.id, Duration::from_secs(10))
            .await?
    );
    let push_ops = c.instances[0].link.stats().ops.load(Ordering::Relaxed) - before;
    ensure!(
        push_ops <= 6,
        "healthy push used {push_ops} requests, budget 6"
    );

    let h = c.instances[0].open(&c.id).await?;
    let before = c.instances[0].link.stats().ops.load(Ordering::Relaxed);
    drop(h.sync_refs().await?);
    let warm_ops = c.instances[0].link.stats().ops.load(Ordering::Relaxed) - before;
    ensure!(
        warm_ops <= 1,
        "warm refs read used {warm_ops} requests, budget 1"
    );

    let cold = c.add_instance("budget-cold", &|_| {});
    let before = c.instances[cold].link.stats().ops.load(Ordering::Relaxed);
    let _h = c.instances[cold].open(&c.id).await?;
    let cold_ops = c.instances[cold].link.stats().ops.load(Ordering::Relaxed) - before;
    ensure!(
        cold_ops <= 2,
        "cold refs sync used {cold_ops} requests, budget 2"
    );
    // Checkpoint: the freshness GET every operation pays, then refs PUT ∥ checkpoint PUT, then
    // the manifest CAS — 4 requests, 3 rounds.
    let before = c.instances[0].link.stats().ops.load(Ordering::Relaxed);
    let cp = h.write_checkpoint().await?;
    let cp_ops = c.instances[0].link.stats().ops.load(Ordering::Relaxed) - before;
    ensure!(
        cp_ops <= 4,
        "checkpoint used {cp_ops} requests, budget 4 (3 rounds: cond GET → PUTs ∥ → CAS)"
    );
    ensure!(
        cp.seq == h.manifest().head_seq,
        "checkpoint at WAL head: {cp:?}"
    );
    eprintln!(
        "healthy request counts: push={push_ops}, warm_refs={warm_ops}, cold_refs={cold_ops}, checkpoint={cp_ops}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Checkpoint writer crashes between its PUTs and the manifest CAS
// ---------------------------------------------------------------------------

/// The checkpoint protocol is refs PUT → checkpoint PUT → manifest CAS (the CAS is the only
/// commit point). A writer that dies after the PUTs leaves orphan objects and an unchanged
/// manifest: a cold reader never sees a half checkpoint, and the next pass writes the same
/// seq idempotently and commits it.
async fn run_checkpoint_crash(seed: u64, crash_at: &str) -> Result<()> {
    let mut c = Cluster::new(seed, 2).await?;
    let mut p = Pusher::new(0);
    for _ in 0..3 {
        ensure!(
            p.push_once(&c.instances[0], &c.id, Duration::from_secs(10))
                .await?
        );
    }
    let before = c.truth_manifest().await?;
    ensure!(before.checkpoint.is_none());
    // The writer's link panics once on the chosen step.
    c.instances[1].link.set(FaultPlan {
        panic_once_keys: vec![crash_at.to_string()],
        ..Default::default()
    });
    let h = c.instances[1].open(&c.id).await?;
    let crashed = tokio::spawn(async move { h.write_checkpoint().await }).await;
    ensure!(
        crashed.is_err() || crashed.as_ref().unwrap().is_err(),
        "the writer should have died at {crash_at}"
    );
    drop(crashed);
    // Truth: manifest unchanged (no checkpoint), whatever objects landed are orphans.
    let after = c.truth_manifest().await?;
    ensure!(
        after.checkpoint.is_none() && after.head_seq == before.head_seq,
        "manifest moved despite the crash"
    );
    // A cold reader sees exactly the pre-crash state.
    let cold = c.add_instance("cold-after-crash", &|_| {});
    let hc = c.instances[cold].open(&c.id).await?;
    drop(hc.sync_refs().await?);
    ensure!(hc.manifest().checkpoint.is_none());
    ensure!(hc.applied_seq() == before.head_seq);
    // The next pass (fresh writer) completes the checkpoint idempotently.
    c.restart(1);
    let h = c.instances[1].open(&c.id).await?;
    let cp = tokio::time::timeout(Duration::from_secs(20), h.write_checkpoint())
        .await
        .map_err(|_| anyhow!("checkpoint hung"))??;
    ensure!(cp.seq == before.head_seq);
    let m = c.truth_manifest().await?;
    ensure!(m.checkpoint.as_ref().map(|x| x.seq) == Some(before.head_seq));
    // The committed checkpoint's objects exist and a cold start folds from it.
    for key in [
        gitcask_proto::keys::checkpoint_key(cp.seq),
        gitcask_proto::keys::checkpoint_refs_key(cp.seq),
    ] {
        ensure!(
            c.truth
                .head(&format!("{}{}", c.repo_prefix(), key))
                .await?
                .is_some(),
            "{key} missing after commit"
        );
    }
    let cold2 = c.add_instance("cold-after-repair", &|_| {});
    let h2 = c.instances[cold2].open(&c.id).await?;
    drop(h2.sync_refs().await?);
    ensure!(h2.manifest().checkpoint.as_ref().map(|x| x.seq) == Some(cp.seq));
    check_truth(&c, std::slice::from_ref(&p)).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sim_checkpoint_writer_crash_is_invisible_and_repaired() {
    for seed in seeds() {
        for crash_at in ["put:checkpoint.pb", "put:manifest.pb"] {
            run_checkpoint_crash(seed, crash_at)
                .await
                .unwrap_or_else(|e| panic!("seed {seed} crash_at {crash_at}: {e:#}"));
        }
    }
}

// ---------------------------------------------------------------------------
// Bucket GC crashes after a conditional delete
// ---------------------------------------------------------------------------

async fn run_gc_crash(seed: u64) -> Result<()> {
    let mut c = Cluster::new(seed, 2).await?;
    let mut p = Pusher::new(0);
    for _ in 0..5 {
        ensure!(
            p.push_once(&c.instances[0], &c.id, Duration::from_secs(10))
                .await?
        );
    }

    let h = c.instances[1].open(&c.id).await?;
    drop(h.sync_full().await?);
    let compacted = gitcask_server::ops::compact_repo(
        &h,
        &c.instances[1].cfg,
        gitcask_server::ops::CompactRequest { force: true },
        &gitcask_server::ops::noop_log,
    )
    .await?;
    ensure!(matches!(
        compacted,
        gitcask_server::ops::CompactOutcome::Published { .. }
    ));
    h.write_checkpoint().await?;

    c.instances[1].link.set(FaultPlan {
        panic_once_keys: vec!["delete:.pack".to_string()],
        ..Default::default()
    });
    let crashed = tokio::spawn(async move {
        gitcask_server::gc::collect(
            h,
            Duration::ZERO,
            Duration::from_hours(30 * 24),
            Duration::from_millis(50),
        )
        .await
    })
    .await;
    ensure!(
        crashed.is_err(),
        "GC must crash during a pack-family delete"
    );
    ensure!(
        c.truth
            .head(&format!("{}{}", c.repo_prefix(), gitcask_proto::keys::GC))
            .await?
            .is_none(),
        "a partial GC must not advance its cursor"
    );

    c.restart(1);
    tokio::time::sleep(Duration::from_millis(60)).await;
    let repaired = c.instances[1].open(&c.id).await?;
    let outcome = gitcask_server::gc::collect(
        repaired,
        Duration::ZERO,
        Duration::from_hours(30 * 24),
        Duration::from_millis(50),
    )
    .await
    .map_err(anyhow::Error::msg)?;
    ensure!(outcome.packs > 0, "restart must finish the partial GC");

    let observer = c.observer();
    let served = observer.open(&c.id).await?;
    let guard = served.sync_full().await?;
    let fsck = served.local().fsck_streaming(true, |_| {}).await?;
    drop(guard);
    ensure!(fsck.ok, "repository failed fsck after GC restart");
    check_truth(&c, std::slice::from_ref(&p)).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sim_gc_crash_is_idempotent_and_serving_recovers() {
    for seed in seeds() {
        run_gc_crash(seed)
            .await
            .unwrap_or_else(|error| panic!("seed {seed}: {error:#}"));
    }
}

// ---------------------------------------------------------------------------
// Task ownership under concurrency + an owner crash; cache pressure
// ---------------------------------------------------------------------------

/// A pusher whose commits carry ~`kb` KB of incompressible content (so packs have bytes to
/// download and caches have something to evict).
fn push_blobby(p: &mut Pusher, kb: usize, rng: &mut Lcg) -> String {
    let mut buf = vec![0u8; kb * 1024];
    for b in buf.iter_mut() {
        *b = rng.next() as u8;
    }
    std::fs::write(p.work.path().join(format!("blob-{}.bin", p.n + 1)), &buf).unwrap();
    p.work.commit(p.n + 1, &format!("p{}", p.idx))
}

/// Task locks are **per instance and in memory** (`Tasks::running`, released by the handle's
/// RAII drop; cross-instance exclusivity is the store lease, not this registry), so "two
/// instances" reduces to many concurrent callers on one instance plus a crash of whoever
/// owns the materialize task. Asserted under randomized link faults: never more than one
/// `materialize` running for the repo; an aborted owner releases the lock at once (the next
/// caller starts its own task — nothing blocks forever); a late joiner's `attach()` replays
/// the story so far and sees the outcome; downloads are not multiplied by the callers.
async fn run_task_ownership(seed: u64) -> Result<()> {
    let mut rng = Lcg(seed);
    let mut c = Cluster::new(seed, 1).await?;
    let mut p = Pusher::new(0);
    for _ in 0..4 {
        let new = push_blobby(&mut p, 64, &mut rng);
        let h = c.instances[0].open(&c.id).await?;
        let pack = p
            .work
            .pack(&new, (!p.tip.is_empty()).then_some(p.tip.as_str()));
        let ingested = h
            .local()
            .ingest_pack(
                std::io::Cursor::new(pack),
                IngestOptions {
                    fsck: false,
                    max_bytes: None,
                    thin: true,
                },
            )
            .await?
            .unwrap();
        let txn = RefTransaction {
            updates: vec![RefUpdate {
                name: p.refname.clone(),
                old_oid: p.tip.clone(),
                new_oid: new.clone(),
                ..Default::default()
            }],
            ..Default::default()
        };
        h.publish_push(Some(ingested), txn, HashMap::new()).await?;
        p.tip = new;
    }
    let live_packs = c.truth_manifest().await?.packs.len();
    ensure!(live_packs >= 4);

    // A fresh instance whose pack reads are slow and flaky.
    let j = c.add_instance("joiners", &|_| {});
    c.instances[j].link.set(
        FaultPlan {
            delay: Some((
                Duration::from_millis(1),
                Duration::from_millis(2 + rng.below(15)),
            )),
            p_err_before: 0.05 + (rng.below(10) as f64) / 100.0,
            p_truncate: 0.05,
            ..Default::default()
        }
        .with_only(&["wal/"]),
    );
    let h = c.instances[j].open(&c.id).await?;
    let tasks = c.instances[j].registry.tasks().clone();
    let repo = c.id.to_string();

    // K concurrent object-level syncs; one random caller is aborted after a random delay.
    let k = 4 + rng.below(4) as usize;
    let mut joins = Vec::new();
    for _ in 0..k {
        let h = h.clone();
        joins.push(tokio::spawn(async move {
            h.sync_full().await.map(drop).map_err(|e| e.to_string())
        }));
    }
    let victim = rng.below(k as u64) as usize;
    let abort_after = Duration::from_millis(rng.below(40));
    // Watch the task registry while they run: at most one materialize task at a time.
    let watcher = {
        let tasks = tasks.clone();
        let repo = repo.clone();
        tokio::spawn(async move {
            let mut max_running = 0usize;
            for _ in 0..400 {
                let n = tasks
                    .running(&repo)
                    .iter()
                    .filter(|t| t.kind == "materialize")
                    .count();
                max_running = max_running.max(n);
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            max_running
        })
    };
    tokio::time::sleep(abort_after).await;
    joins[victim].abort();
    let mut errors = 0usize;
    for (i, jh) in joins.into_iter().enumerate() {
        match tokio::time::timeout(Duration::from_secs(30), jh).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(_))) => errors += 1,
            Ok(Err(e)) if e.is_cancelled() && i == victim => errors += 1,
            Ok(Err(e)) => bail!("caller {i} panicked: {e}"),
            Err(_) => bail!(
                "caller {i} hung 30 s: a task lock was never released\n{}",
                c.dump_traces()
            ),
        }
    }
    let max_running = watcher.await?;
    ensure!(
        max_running <= 1,
        "{max_running} materialize tasks ran concurrently for one repo"
    );

    // Afterwards: nothing stuck, and a healed caller completes (or had completed).
    c.instances[j].link.heal();
    tokio::time::timeout(Duration::from_secs(30), h.sync_full())
        .await
        .map_err(|_| anyhow!("sync hung after the chaos\n{}", c.dump_traces()))??;
    ensure!(h.packs_ready(), "packs not ready after a healthy sync");
    ensure!(
        tasks.running(&repo).is_empty(),
        "a task is still marked running: {:?}",
        tasks.running(&repo)
    );
    let recent = tasks.recent(&repo);
    let materializes: Vec<_> = recent.iter().filter(|t| t.kind == "materialize").collect();
    ensure!(!materializes.is_empty());
    // Every start beyond the first is accounted for by a failure or the abort — never a duplicate.
    ensure!(
        materializes.len() <= 1 + errors + 1,
        "{} materialize tasks for {k} callers with {errors} failures:\n{:?}",
        materializes.len(),
        materializes
            .iter()
            .map(|t| (&t.id, &t.summary))
            .collect::<Vec<_>>()
    );
    // A late joiner attaches to the finished task and gets the replay + outcome.
    let last_ok = materializes
        .iter()
        .find(|t| t.ok == Some(true))
        .ok_or_else(|| anyhow!("no successful materialize: {materializes:?}"))?;
    let state = tasks
        .get(&last_ok.id)
        .ok_or_else(|| anyhow!("task state gone"))?;
    let (replay, _rx, outcome) = state.attach();
    ensure!(!replay.is_empty(), "late joiner got no replay");
    ensure!(
        matches!(outcome, Some(Ok(_))),
        "late joiner did not see the outcome: {outcome:?}"
    );
    // Downloads: every attempt downloads each pack at most once (+ idx); no N-fold traffic.
    let ops = c.instances[j].link.stats().ops.load(Ordering::Relaxed) as usize;
    let attempts = materializes.len();
    let budget = attempts * (live_packs * 4 + 6) + k * 3 + 20;
    ensure!(
        ops <= budget,
        "{ops} store requests for {attempts} materialize attempt(s) over {live_packs} packs (budget {budget})"
    );
    check_truth(&c, std::slice::from_ref(&p)).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sim_task_ownership_under_concurrency_and_owner_crash() {
    for seed in seeds() {
        run_task_ownership(seed)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: {e:#}"));
    }
}
