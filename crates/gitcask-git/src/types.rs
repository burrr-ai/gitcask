use std::path::PathBuf;

use crate::GitError;

#[derive(Debug, Clone)]
pub struct IngestOptions {
    pub fsck: bool,
    pub max_bytes: Option<u64>,
    pub thin: bool,
}

#[derive(Debug, Clone)]
pub struct IngestedPack {
    pub checksum: gix_hash::ObjectId,
    pub pack_path: PathBuf,
    pub idx_path: PathBuf,
    pub pack_size: u64,
    pub idx_size: u64,
    pub object_count: u64,
}

#[derive(Debug, Clone)]
pub struct PackInfo {
    pub checksum: gix_hash::ObjectId,
    pub pack_size: u64,
    pub idx_size: u64,
    pub object_count: u64,
    pub has_rev: bool,
    pub has_bitmap: bool,
    /// `pack-<checksum>.commit-graph` side-file: a split commit-graph layer
    /// covering the pack's commits (see [`crate::LocalRepo::write_pack_commit_graph`]).
    pub has_commit_graph: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    pub name: String,
    pub oid: String,
    pub peeled: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefSnapshotData {
    pub refs: Vec<Ref>,
    pub head_target: String,
}

impl From<gitcask_proto::v1::RefSnapshot> for RefSnapshotData {
    fn from(s: gitcask_proto::v1::RefSnapshot) -> Self {
        let refs = s
            .refs
            .into_iter()
            .map(|r| Ref {
                name: r.name,
                oid: r.oid,
                peeled: r.peeled,
            })
            .collect();
        RefSnapshotData {
            refs,
            head_target: s.head_target,
        }
    }
}

impl From<RefSnapshotData> for gitcask_proto::v1::RefSnapshot {
    fn from(d: RefSnapshotData) -> Self {
        let refs = d
            .refs
            .into_iter()
            .map(|r| gitcask_proto::v1::Ref {
                name: r.name,
                oid: r.oid,
                peeled: r.peeled,
            })
            .collect();
        gitcask_proto::v1::RefSnapshot {
            seq: 0,
            object_format: "sha1".into(),
            refs,
            head_target: d.head_target,
            created_at: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct LsRefsArgs {
    pub ref_prefixes: Vec<String>,
    pub symrefs: bool,
    pub peel: bool,
    pub unborn: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LsRefsLine {
    pub name: String,
    pub oid: String,
    pub peeled: String,
    pub symref_target: Option<String>,
}

impl LsRefsLine {
    /// Render the line per protocol-v2 ls-refs format:
    /// `<oid> <name>` optionally followed by ` symref-target:<t>` and/or
    /// ` peeled:<oid>`, then a trailing newline.
    pub fn render(&self, args: &LsRefsArgs) -> String {
        let mut s = format!("{} {}", self.oid, self.name);
        if args.symrefs || self.oid == "unborn" {
            if let Some(t) = &self.symref_target {
                s.push_str(&format!(" symref-target:{t}"));
            }
        }
        if args.peel && !self.peeled.is_empty() {
            s.push_str(&format!(" peeled:{}", self.peeled));
        }
        s.push('\n');
        s
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    UploadPack,
    ReceivePack,
}

impl std::str::FromStr for Service {
    type Err = GitError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "git-upload-pack" => Ok(Service::UploadPack),
            "git-receive-pack" => Ok(Service::ReceivePack),
            other => Err(GitError::InvalidInput(format!("unknown service {other}"))),
        }
    }
}

impl Service {
    pub fn as_str(&self) -> &'static str {
        match self {
            Service::UploadPack => "git-upload-pack",
            Service::ReceivePack => "git-receive-pack",
        }
    }
}

#[derive(Debug, Clone)]
pub struct UploadPackRequest {
    pub wants: Vec<gix_hash::ObjectId>,
    pub haves: Vec<gix_hash::ObjectId>,
    pub done: bool,
    pub thin_pack: bool,
    pub no_progress: bool,
    pub include_tag: bool,
    pub ofs_delta: bool,
    pub sideband_all: bool,
    pub wait_for_done: bool,
    pub filter: Option<String>,
    pub deepen: Option<u32>,
    pub deepen_since: Option<i64>,
    pub deepen_not: Vec<String>,
    pub shallow: Vec<gix_hash::ObjectId>,
    pub want_refs: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum RepackMode {
    Geometric { factor: u32 },
    Full,
}

#[derive(Debug, Clone)]
pub struct RepackOptions {
    pub mode: RepackMode,
    pub write_bitmap: bool,
    pub write_midx: bool,
    pub keep: Vec<gix_hash::ObjectId>,
}

#[derive(Debug, Clone, Default)]
pub struct RepackResult {
    pub new_packs: Vec<PackInfo>,
    pub removed: Vec<gix_hash::ObjectId>,
}

/// Outcome of [`crate::LocalRepo::fsck_streaming`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct FsckReport {
    pub ok: bool,
    pub exit_code: Option<i32>,
    pub problems: u64,
}
