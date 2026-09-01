use std::collections::HashSet;
use std::process::Stdio;

use gix_object::{FindExt, FindHeader};
use gix_traverse::tree::Visit as TreeVisit;

use crate::error::ge;
use crate::{FsckReport, GitError, LocalRepo};

impl LocalRepo {
    pub fn has_object(&self, oid: &gix_hash::oid) -> bool {
        let repo = self.gix();
        repo.has_object(oid)
    }

    /// Every object reachable from tips exists. When stop_at_existing_refs,
    /// objects already reachable from current refs are assumed present and
    /// only the new set is verified. Uses gix revwalk with .with_hidden(
    /// existing ref tips) for commit traversal and gix_traverse::tree
    /// breadthfirst for tree traversal with a seen-set.
    pub fn check_connectivity(
        &self,
        tips: &[gix_hash::ObjectId],
        stop_at_existing_refs: bool,
    ) -> Result<(), GitError> {
        let span = tracing::info_span!(
            "git.check_connectivity",
            repo = %self.inner.id,
            tips = tips.len(),
            stop_at_existing_refs,
        );
        let _enter = span.enter();

        // Mirror pushes commonly contain thousands of refs at the same tip.
        // Avoid an O(number-of-updates) object lookup and duplicate rev-walk
        // roots for those requests.
        let mut unique_tips = Vec::with_capacity(tips.len());
        let mut tip_set = HashSet::with_capacity(tips.len());
        for tip in tips {
            if tip_set.insert(*tip) {
                unique_tips.push(*tip);
            }
        }
        if unique_tips.is_empty() {
            return Ok(());
        }

        let repo = self.gix();
        let mut seen: HashSet<gix_hash::ObjectId> = HashSet::new();
        let mut buf = Vec::new();
        let mut tree_state = gix_traverse::tree::breadthfirst::State::default();

        // Tips may be commits, annotated tags (possibly nested), trees or blobs.
        // Peel tags (verifying every object in the chain exists), walk trees and
        // check blobs directly; only commits seed the rev-walk.
        let mut commit_tips = Vec::with_capacity(unique_tips.len());
        for t in &unique_tips {
            match peel_tip(&repo, *t, &mut seen, &mut buf)? {
                (gix_object::Kind::Commit, id) => commit_tips.push(id),
                (gix_object::Kind::Tree, id) => {
                    if seen.insert(id) {
                        let tree_iter = repo
                            .objects
                            .find_tree_iter(&id, &mut buf)
                            .map_err(|e| GitError::Gix(Box::new(e)))?;
                        let mut visitor = ConnectivityVisitor {
                            seen: &mut seen,
                            repo: &repo,
                            missing: None,
                        };
                        if let Err(e) = gix_traverse::tree::breadthfirst(
                            tree_iter,
                            &mut tree_state,
                            &repo.objects,
                            &mut visitor,
                        ) {
                            return Err(match visitor.missing {
                                Some(oid) => GitError::MissingObject {
                                    oid: oid.to_hex().to_string(),
                                },
                                None => GitError::Gix(Box::new(e)),
                            });
                        }
                    }
                }
                (_, _) => {}
            }
        }
        if commit_tips.is_empty() {
            return Ok(());
        }

        // Collect hidden tips (existing ref tips, peeled to commits) for stop-at-existing.
        let hidden: Vec<gix_hash::ObjectId> = if stop_at_existing_refs {
            let snap = self.refs()?;
            let mut seen_hidden = HashSet::with_capacity(snap.refs.len());
            let mut out = Vec::with_capacity(snap.refs.len());
            for r in &snap.refs {
                // Prefer the pre-peeled oid for tags; otherwise peel cheaply via the odb.
                let candidate = if !r.peeled.is_empty() {
                    r.peeled.as_str()
                } else {
                    r.oid.as_str()
                };
                let Ok(oid) = gix_hash::ObjectId::from_hex(candidate.as_bytes()) else {
                    continue;
                };
                if !seen_hidden.insert(oid) {
                    continue;
                }
                match repo.objects.try_header(&oid) {
                    Ok(Some(h)) if h.kind == gix_object::Kind::Commit => out.push(oid),
                    Ok(Some(h)) if h.kind == gix_object::Kind::Tag => {
                        let mut ignore = HashSet::new();
                        if let Ok((gix_object::Kind::Commit, id)) =
                            peel_tip(&repo, oid, &mut ignore, &mut buf)
                        {
                            out.push(id);
                        }
                    }
                    _ => {}
                }
            }
            out
        } else {
            Vec::new()
        };

        // Walk commits from tips, hiding existing ref tips.
        let walk = repo
            .rev_walk(commit_tips)
            .with_hidden(hidden.iter().copied())
            .all()
            .map_err(|e| GitError::Gix(Box::new(e)))?;

        for item in walk {
            let info = item.map_err(|e| GitError::Gix(Box::new(e)))?;
            let cid = info.id;
            if !seen.insert(cid) {
                continue;
            }
            if !repo.has_object(&cid) {
                return Err(GitError::MissingObject {
                    oid: cid.to_hex().to_string(),
                });
            }
            // Get the commit's tree id.
            let mut commit = repo
                .objects
                .find_commit_iter(&cid, &mut buf)
                .map_err(|e| GitError::Gix(Box::new(e)))?;
            let tree_id = commit.tree_id().map_err(|e| ge(e))?;
            if seen.insert(tree_id) {
                if !repo.has_object(&tree_id) {
                    return Err(GitError::MissingObject {
                        oid: tree_id.to_hex().to_string(),
                    });
                }
                let tree_iter = repo
                    .objects
                    .find_tree_iter(&tree_id, &mut buf)
                    .map_err(|e| GitError::Gix(Box::new(e)))?;
                let mut visitor = ConnectivityVisitor {
                    seen: &mut seen,
                    repo: &repo,
                    missing: None,
                };
                if let Err(e) = gix_traverse::tree::breadthfirst(
                    tree_iter,
                    &mut tree_state,
                    &repo.objects,
                    &mut visitor,
                ) {
                    return Err(match visitor.missing {
                        Some(oid) => GitError::MissingObject {
                            oid: oid.to_hex().to_string(),
                        },
                        None => GitError::Gix(Box::new(e)),
                    });
                }
            }
        }
        Ok(())
    }

    /// [`check_connectivity`] on a blocking thread. The walk inflates trees
    /// against the pack set (5.5 s on a 2,852-commit push) and
    /// must not sit on a tokio worker.
    pub async fn check_connectivity_async(
        &self,
        tips: &[gix_hash::ObjectId],
        stop_at_existing_refs: bool,
    ) -> Result<(), GitError> {
        let this = self.clone();
        let tips = tips.to_vec();
        tokio::task::spawn_blocking(move || this.check_connectivity(&tips, stop_at_existing_refs))
            .await
            .map_err(|e| GitError::Protocol(format!("connectivity task: {e}")))?
    }
    /// Full `git fsck` of the local copy, streaming every output line (stdout
    /// and stderr, interleaved by arrival) to `on_line`. `connectivity_only`
    /// skips object content checks (much faster on big repos). Returns the
    /// number of problem lines git printed; `Err` only when git itself failed
    /// to run.
    pub async fn fsck_streaming(
        &self,
        connectivity_only: bool,
        mut on_line: impl FnMut(String) + Send,
    ) -> Result<FsckReport, GitError> {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut args = vec![
            "fsck",
            "--full",
            "--strict",
            "--no-progress",
            "--no-dangling",
        ];
        if connectivity_only {
            args.push("--connectivity-only");
        }
        let mut child = tokio::process::Command::new("git")
            .current_dir(&self.inner.path)
            .env("GIT_DIR", &self.inner.path)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(GitError::Io)?;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(256);
        let mut readers = Vec::new();
        if let Some(out) = child.stdout.take() {
            let tx = tx.clone();
            readers.push(tokio::spawn(async move {
                let mut lines = BufReader::new(out).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    if tx.send(l).await.is_err() {
                        break;
                    }
                }
            }));
        }
        if let Some(err) = child.stderr.take() {
            readers.push(tokio::spawn(async move {
                let mut lines = BufReader::new(err).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    if tx.send(l).await.is_err() {
                        break;
                    }
                }
            }));
        }
        let mut problems = 0u64;
        while let Some(line) = rx.recv().await {
            let l = line.trim_end().to_string();
            if l.is_empty() {
                continue;
            }
            let lower = l.to_ascii_lowercase();
            if lower.starts_with("error")
                || lower.starts_with("missing")
                || lower.starts_with("broken")
                || lower.starts_with("unreachable")
                || lower.contains("fatal:")
            {
                problems += 1;
            }
            on_line(l);
        }
        for r in readers {
            let _ = r.await;
        }
        let status = child.wait().await.map_err(GitError::Io)?;
        Ok(FsckReport {
            ok: status.success() && problems == 0,
            exit_code: status.code(),
            problems,
        })
    }
}

/// Tree visitor for connectivity checking: records every referenced tree
/// and non-tree (blob/gitlink) oid in a seen-set and verifies existence.
/// Follow a tip through annotated tags, verifying each object exists, and return
/// the kind and id of the final non-tag object.
fn peel_tip(
    repo: &gix::Repository,
    mut id: gix_hash::ObjectId,
    seen: &mut HashSet<gix_hash::ObjectId>,
    buf: &mut Vec<u8>,
) -> Result<(gix_object::Kind, gix_hash::ObjectId), GitError> {
    use gix_object::Find;
    for _ in 0..64 {
        let data = repo
            .objects
            .try_find(&id, buf)
            .map_err(GitError::Gix)?
            .ok_or_else(|| GitError::MissingObject {
                oid: id.to_hex().to_string(),
            })?;
        if data.kind != gix_object::Kind::Tag {
            return Ok((data.kind, id));
        }
        seen.insert(id);
        let tag = gix_object::TagRefIter::from_bytes(data.data, repo.object_hash());
        let target = tag.target_id().map_err(|e| GitError::Gix(Box::new(e)))?;
        id = target;
    }
    Err(GitError::InvalidInput(format!(
        "tag chain too deep at {}",
        id.to_hex()
    )))
}

struct ConnectivityVisitor<'a> {
    seen: &'a mut HashSet<gix_hash::ObjectId>,
    repo: &'a gix::Repository,
    /// First object found missing (reported as `MissingObject`).
    missing: Option<gix_hash::ObjectId>,
}

impl<'a> TreeVisit for ConnectivityVisitor<'a> {
    fn pop_front_tracked_path_and_set_current(&mut self) {}
    fn pop_back_tracked_path_and_set_current(&mut self) {}
    fn push_back_tracked_path_component(&mut self, _c: &gix_object::bstr::BStr) {}
    fn push_path_component(&mut self, _c: &gix_object::bstr::BStr) {}
    fn pop_path_component(&mut self) {}

    fn visit_tree(
        &mut self,
        entry: &gix_object::tree::EntryRef<'_>,
    ) -> gix_traverse::tree::visit::Action {
        // Already verified (shared subtree): do not descend again. On a
        // monorepo this is the difference between "changed paths" and "4M files".
        if !self.seen.insert(entry.oid.to_owned()) {
            return std::ops::ControlFlow::Continue(false);
        }
        if !self.repo.has_object(entry.oid) {
            self.missing = Some(entry.oid.to_owned());
            return std::ops::ControlFlow::Break(());
        }
        std::ops::ControlFlow::Continue(true)
    }

    fn visit_nontree(
        &mut self,
        entry: &gix_object::tree::EntryRef<'_>,
    ) -> gix_traverse::tree::visit::Action {
        // Submodule entries (gitlinks) point at commits of *another* repository;
        // they are never expected to exist here (git/git: sha1collisiondetection).
        if entry.mode.is_commit() {
            return std::ops::ControlFlow::Continue(true);
        }
        if self.seen.insert(entry.oid.to_owned()) {
            if !self.repo.has_object(entry.oid) {
                self.missing = Some(entry.oid.to_owned());
                return std::ops::ControlFlow::Break(());
            }
        }
        std::ops::ControlFlow::Continue(true)
    }
}
