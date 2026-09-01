use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::proc::rename_atomic;
use crate::{GitError, LocalRepo, PackInfo, RepackMode, RepackOptions, RepackResult};

impl LocalRepo {
    /// `git merge-base --is-ancestor old new`: true when `new` is a descendant of `old`.
    /// Missing objects or non-commit oids return `Ok(false)` (treat as not fast-forward).
    pub async fn is_ancestor(&self, old: &str, new: &str) -> Result<bool, GitError> {
        let out = self.git(&["merge-base", "--is-ancestor", old, new]).await?;
        Ok(out.status.success())
    }

    pub async fn repack(&self, opts: RepackOptions) -> Result<RepackResult, GitError> {
        let before: HashSet<gix_hash::ObjectId> =
            self.packs()?.into_iter().map(|p| p.checksum).collect();

        let mut args: Vec<String> = vec!["repack".into()];
        match opts.mode {
            RepackMode::Geometric { factor } => {
                args.push("-d".into());
                args.push(format!("--geometric={factor}"));
                if opts.write_midx {
                    args.push("--write-midx".into());
                }
                if opts.write_bitmap {
                    args.push("--write-bitmap-index".into());
                }
            }
            RepackMode::Full => {
                args.push("-a".into());
                args.push("-d".into());
                // Every core for the delta phase; the write + bitmap phases are
                // single-threaded in git (a large repository's base: 16 min for 32 GB on 44
                // cores, 2026-08-21 dry run — mostly that).
                args.push("--threads=0".into());
                if opts.write_bitmap {
                    args.push("--write-bitmap-index".into());
                }
                if opts.write_midx {
                    args.push("--write-midx".into());
                }
            }
        }
        // `--keep-pack=<file>` excludes a pack from the repack (git's `.keep` semantics for one
        // run). (`--keep=<hex>` was ambiguous between --keep-unreachable and --keep-pack and never
        // worked; no caller passed a non-empty list until 2026-08-22.)
        for k in &opts.keep {
            args.push(format!("--keep-pack=pack-{}.pack", k.to_hex()));
        }
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let out = self.git(&arg_refs).await?;
        if !out.status.success() {
            return Err(GitError::Subprocess {
                cmd: "git repack".into(),
                status: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        self.refresh_async().await?;

        let after = self.packs()?;
        let new_packs: Vec<PackInfo> = after
            .iter()
            .filter(|p| !before.contains(&p.checksum))
            .cloned()
            .collect();
        let removed: Vec<gix_hash::ObjectId> = before
            .into_iter()
            .filter(|c| !after.iter().any(|p| &p.checksum == c))
            .collect();
        Ok(RepackResult { new_packs, removed })
    }

    // ---- commit-graph ----

    fn commit_graphs_dir(&self) -> PathBuf {
        self.inner
            .path
            .join("objects")
            .join("info")
            .join("commit-graphs")
    }

    /// Build a single split commit-graph layer for every reachable commit
    /// (`git commit-graph write --reachable --split=replace [--changed-paths]`)
    /// and copy it next to `checksum`'s pack as `pack-<checksum>.commit-graph`,
    /// the side-file published with the pack. Returns the layer size.
    pub async fn write_pack_commit_graph(
        &self,
        checksum: &gix_hash::oid,
        changed_paths: bool,
    ) -> Result<u64, GitError> {
        let mut args = vec!["commit-graph", "write", "--reachable", "--split=replace"];
        if changed_paths {
            args.push("--changed-paths");
        }
        let out = self.git(&args).await?;
        if !out.status.success() {
            return Err(GitError::Subprocess {
                cmd: "git commit-graph write".into(),
                status: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        let layers = self.commit_graph_chain()?;
        let Some(hash) = layers.last() else {
            return Err(GitError::InvalidInput(
                "commit-graph write produced no layer".into(),
            ));
        };
        let src = self.commit_graphs_dir().join(format!("graph-{hash}.graph"));
        let dst = self.pack_path(checksum).with_extension("commit-graph");
        let tmp = dst.with_extension("commit-graph.tmp");
        std::fs::copy(&src, &tmp).map_err(GitError::Io)?;
        rename_atomic(&tmp, &dst)?;
        Ok(std::fs::metadata(&dst).map(|m| m.len()).unwrap_or(0))
    }

    /// Hashes listed in `objects/info/commit-graphs/commit-graph-chain`
    /// (base first), empty when there is no chain.
    pub fn commit_graph_chain(&self) -> Result<Vec<String>, GitError> {
        match std::fs::read_to_string(self.commit_graphs_dir().join("commit-graph-chain")) {
            Ok(s) => Ok(s
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(GitError::Io(e)),
        }
    }

    /// Install `pack-<checksum>.commit-graph` as the *base* layer of the
    /// repository's commit-graph chain: `commit-graphs/graph-<hash>.graph` +
    /// a chain file naming only that layer (a monolithic
    /// `objects/info/commit-graph` is removed — git would otherwise fold it
    /// into a full rewrite on the next `--split` write). Returns false when
    /// the side-file is absent. No-op when it already heads the chain.
    pub fn install_commit_graph_base(&self, checksum: &gix_hash::oid) -> Result<bool, GitError> {
        let side = self.pack_path(checksum).with_extension("commit-graph");
        if !side.exists() {
            return Ok(false);
        }
        let hash = commit_graph_layer_hash(&side)?;
        let chain = self.commit_graph_chain()?;
        if chain.first().map(|h| h == &hash).unwrap_or(false) {
            return Ok(true);
        }
        let dir = self.commit_graphs_dir();
        std::fs::create_dir_all(&dir).map_err(GitError::Io)?;
        let layer = dir.join(format!("graph-{hash}.graph"));
        if !layer.exists() {
            let tmp = dir.join(format!("graph-{hash}.graph.tmp"));
            if std::fs::hard_link(&side, &tmp).is_err() {
                std::fs::copy(&side, &tmp).map_err(GitError::Io)?;
            }
            rename_atomic(&tmp, &layer)?;
        }
        let chain_tmp = dir.join("commit-graph-chain.tmp");
        std::fs::write(&chain_tmp, format!("{hash}\n")).map_err(GitError::Io)?;
        rename_atomic(&chain_tmp, &dir.join("commit-graph-chain"))?;
        // Old layers are unreferenced now; drop them (never the new base).
        for old in chain.iter().filter(|h| **h != hash) {
            let _ = std::fs::remove_file(dir.join(format!("graph-{old}.graph")));
        }
        let mono = self
            .inner
            .path
            .join("objects")
            .join("info")
            .join("commit-graph");
        match std::fs::remove_file(&mono) {
            Ok(()) | Err(_) => {}
        }
        self.refresh()?;
        Ok(true)
    }

    /// Add the commits of `packs` (local pack checksums) to the commit-graph
    /// chain as a new tip layer (`git commit-graph write --split
    /// --stdin-packs`). Commits already in the chain are skipped by git;
    /// generation numbers come from the existing layers, so with a base layer
    /// installed this never reads base pack data (unless `changed_paths`,
    /// which diffs against parent trees). Cheap and incremental; git merges
    /// layers geometrically.
    pub async fn update_commit_graph(
        &self,
        packs: &[gix_hash::ObjectId],
        changed_paths: bool,
    ) -> Result<(), GitError> {
        if packs.is_empty() {
            return Ok(());
        }
        let mut input = String::new();
        for p in packs {
            input.push_str(&format!("pack-{}.idx\n", p.to_hex()));
        }
        let mut args = vec!["write", "--split", "--stdin-packs"];
        if changed_paths {
            args.push("--changed-paths");
        }
        let out = self
            .run_git_stdin("commit-graph", &args, input.as_bytes())
            .await?;
        if !out.status.success() {
            return Err(GitError::Subprocess {
                cmd: "git commit-graph write --split".into(),
                status: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        self.refresh_async().await?;
        Ok(())
    }
}

/// The hash git names a split commit-graph layer by: the file's trailing
/// checksum (hash size from the header's hash-version byte).
fn commit_graph_layer_hash(path: &Path) -> Result<String, GitError> {
    let data = std::fs::read(path).map_err(GitError::Io)?;
    // Header: "CGPH" version(1) hash-version(1) chunks(1) base-graphs(1)
    if data.len() < 8 || &data[..4] != b"CGPH" {
        return Err(GitError::InvalidInput(format!(
            "{} is not a commit-graph",
            path.display()
        )));
    }
    let len = if data[5] == 2 { 32 } else { 20 };
    if data.len() < 8 + len {
        return Err(GitError::InvalidInput(format!(
            "{} is truncated",
            path.display()
        )));
    }
    Ok(hex::encode(&data[data.len() - len..]))
}

/// Derive a pack's reverse index (`.rev`, RIDX v1) from its `.idx`: header
/// (`RIDX`, version 1, hash id), N × u32 BE index positions sorted by pack
/// offset, the pack checksum (from the idx trailer), and the checksum of the
/// file. Byte-identical to `git index-pack --rev-index` / `pack.writeReverseIndex`
/// (git's `write_rev_index_positions` sorts by offset with the index position
/// as the tiebreaker, which cannot occur: offsets are unique). Written to a
/// temp name and renamed, so a reader never sees a partial file.
pub fn write_rev_from_idx(
    idx_path: &Path,
    rev_path: &Path,
    kind: gix_hash::Kind,
) -> Result<(), GitError> {
    let index =
        gix_pack::index::File::at(idx_path, kind).map_err(|e| GitError::Gix(Box::new(e)))?;
    let n = index.num_objects();
    let mut by_offset: Vec<(u64, u32)> = index
        .iter()
        .enumerate()
        .map(|(i, e)| (e.pack_offset, i as u32))
        .collect();
    by_offset.sort_unstable();
    let mut out = Vec::with_capacity(12 + 4 * n as usize + 2 * kind.len_in_bytes());
    out.extend_from_slice(b"RIDX");
    out.extend_from_slice(&1u32.to_be_bytes());
    out.extend_from_slice(
        &(if kind == gix_hash::Kind::Sha1 {
            1u32
        } else {
            2u32
        })
        .to_be_bytes(),
    );
    for (_, pos) in &by_offset {
        out.extend_from_slice(&pos.to_be_bytes());
    }
    out.extend_from_slice(index.pack_checksum().as_bytes());
    let mut h = gix_hash::hasher(kind);
    h.update(&out);
    let trailer = h
        .try_finalize()
        .map_err(|e| GitError::InvalidInput(format!("rev checksum: {e}")))?;
    out.extend_from_slice(trailer.as_bytes());
    let tmp = rev_path.with_extension("rev.tmp");
    std::fs::write(&tmp, &out).map_err(GitError::Io)?;
    std::fs::rename(&tmp, rev_path).map_err(GitError::Io)?;
    Ok(())
}
