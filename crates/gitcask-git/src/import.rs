//! Streaming import packing shared by the CLI and HTTP engine. No checkout,
//! alternates in the destination, or source mutations.
use crate::{GitError, IngestOptions, IngestedPack, LocalRepo};
use std::{path::Path, process::Stdio};
use tokio::io::AsyncWriteExt;

/// Trusted host Git, with inherited configuration, hooks, helpers and protocols
/// disabled. HTTP is permitted only for the server's scoped loopback relay.
pub fn isolated_command(path: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("git");
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .current_dir(path)
        .args([
            "-c",
            "credential.helper=",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "protocol.allow=never",
            "-c",
            "protocol.http.allow=always",
            "-c",
            "http.followRedirects=false",
            "-c",
            "http.proxy=",
            "-c",
            "gc.auto=0",
            "-c",
            "maintenance.auto=false",
            "-c",
            "fetch.fsckObjects=true",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command
}

impl LocalRepo {
    /// Pack either every source ref (CLI), or a pinned explicit closure (HTTP),
    /// then stream it through the same index-pack ingestion as receive-pack.
    pub async fn import_pack_from(
        &self,
        source: &Path,
        tips: Option<&[String]>,
        options: IngestOptions,
    ) -> Result<Option<IngestedPack>, GitError> {
        self.import_pack_impl(source, tips, options, false).await
    }

    /// Import ingestion whose producer and indexer die when the future is
    /// cancelled. Used by deadline-bounded HTTP acquisition, not receive-pack.
    pub async fn import_pack_from_supervised(
        &self,
        source: &Path,
        tips: Option<&[String]>,
        options: IngestOptions,
    ) -> Result<Option<IngestedPack>, GitError> {
        self.import_pack_impl(source, tips, options, true).await
    }
    async fn import_pack_impl(
        &self,
        source: &Path,
        tips: Option<&[String]>,
        options: IngestOptions,
        supervised: bool,
    ) -> Result<Option<IngestedPack>, GitError> {
        let mut command = isolated_command(source);
        command.args(["pack-objects", "--stdout"]);
        if tips.is_some() {
            command.arg("--revs").stdin(Stdio::piped());
        } else {
            // Preserve the standalone CLI importer's Git progress output.
            command.arg("--all").stderr(Stdio::inherit());
        }
        configure_group(&mut command);
        let mut child = command.spawn().map_err(GitError::Io)?;
        let mut group = crate::ImportProcess::new(child.id());
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| GitError::Io(std::io::Error::other("pack stdout")))?;
        let feed = async {
            if let Some(tips) = tips {
                let mut input = child
                    .stdin
                    .take()
                    .ok_or_else(|| GitError::Io(std::io::Error::other("pack stdin")))?;
                for tip in tips {
                    crate::validate_oid(tip)?;
                    input
                        .write_all(format!("{tip}\n").as_bytes())
                        .await
                        .map_err(GitError::Io)?;
                }
                input.shutdown().await.map_err(GitError::Io)?;
            }
            Ok::<_, GitError>(())
        };
        let ingest = async {
            if supervised {
                self.index_import_stream(stdout, options).await
            } else {
                self.ingest_pack(stdout, options).await
            }
        };
        let ((), pack) = tokio::try_join!(feed, ingest)?;
        let status = child.wait().await.map_err(GitError::Io)?;
        if !status.success() {
            return Err(GitError::Subprocess {
                cmd: "git pack-objects import".into(),
                status: status.code(),
                stderr: "source closure unavailable".into(),
            });
        }
        group.finished();
        Ok(pack)
    }
}

fn configure_group(command: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
}

impl LocalRepo {
    async fn index_import_stream<R: tokio::io::AsyncRead + Unpin + Send>(
        &self,
        stream: R,
        options: IngestOptions,
    ) -> Result<Option<IngestedPack>, GitError> {
        let spool = self.spool_pack(stream, options.max_bytes).await?;
        if spool.bytes() == 0 {
            return Ok(None);
        }
        let _guard = self.inner.ingest_lock.lock().await;
        let scratch = tokio::task::spawn_blocking({
            let path = self.path().to_path_buf();
            move || {
                tempfile::Builder::new()
                    .prefix("gitcask-import-index-")
                    .tempdir_in(path)
            }
        })
        .await
        .map_err(|e| GitError::Io(std::io::Error::other(e)))?
        .map_err(GitError::Io)?;
        let index_dir = scratch.path().to_path_buf();
        for directory in ["objects/pack", "objects/info", "refs"] {
            tokio::fs::create_dir_all(index_dir.join(directory))
                .await
                .map_err(GitError::Io)?;
        }
        tokio::fs::write(index_dir.join("HEAD"), "ref: refs/heads/main\n")
            .await
            .map_err(GitError::Io)?;
        tokio::fs::copy(self.path().join("config"), index_dir.join("config"))
            .await
            .map_err(GitError::Io)?;
        tokio::fs::write(
            index_dir.join("objects/info/alternates"),
            format!("{}\n", self.path().join("objects").display()),
        )
        .await
        .map_err(GitError::Io)?;
        let mut input = spool.file;
        // Seeking a received spool does not copy bytes or spawn an unbounded
        // blocking Git operation; the native child reads this file descriptor.
        std::io::Seek::rewind(&mut input).map_err(GitError::Io)?;
        let mut command = isolated_command(&index_dir);
        command
            .env("GIT_DIR", &index_dir)
            .args([
                "index-pack",
                "--stdin",
                "--keep",
                "--rev-index",
                "--threads=0",
            ])
            .stdin(Stdio::from(input));
        if options.fsck {
            command.arg("--fsck-objects");
        }
        if options.thin {
            command.arg("--fix-thin");
        }
        self.finish_import_index(scratch, command).await
    }

    async fn finish_import_index(
        &self,
        scratch: tempfile::TempDir,
        mut command: tokio::process::Command,
    ) -> Result<Option<IngestedPack>, GitError> {
        use tokio::io::AsyncReadExt;
        configure_group(&mut command);
        let mut child = command.spawn().map_err(GitError::Io)?;
        let mut group = crate::ImportProcess::new(child.id());
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| GitError::Io(std::io::Error::other("import index stdout")))?
            .take(513);
        let mut output = Vec::new();
        stdout
            .read_to_end(&mut output)
            .await
            .map_err(GitError::Io)?;
        if output.len() > 512 {
            return Err(GitError::Protocol("index-pack output exceeds bound".into()));
        }
        let status = child.wait().await.map_err(GitError::Io)?;
        if !status.success() {
            return Err(GitError::Subprocess {
                cmd: "git index-pack import".into(),
                status: status.code(),
                stderr: "indexing failed".into(),
            });
        }
        group.finished();
        let checksum = String::from_utf8_lossy(&output)
            .split_whitespace()
            .find_map(|s| gix_hash::ObjectId::from_hex(s.as_bytes()).ok())
            .ok_or_else(|| GitError::Protocol("index-pack returned no checksum".into()))?;
        let hex = checksum.to_hex();
        let index_dir = scratch.path().join("objects/pack");
        let destination = self.path().join("objects/pack");
        for ext in ["idx", "rev", "pack"] {
            let name = format!("pack-{hex}.{ext}");
            let from = index_dir.join(&name);
            if tokio::fs::try_exists(&from).await.map_err(GitError::Io)? {
                tokio::fs::rename(&from, destination.join(name))
                    .await
                    .map_err(GitError::Io)?;
            }
        }
        let pack_path = destination.join(format!("pack-{hex}.pack"));
        let idx_path = pack_path.with_extension("idx");
        let object_count = tokio::task::spawn_blocking({
            let idx = idx_path.clone();
            move || crate::ingest::idx_object_count(&idx)
        })
        .await
        .map_err(|e| GitError::Io(std::io::Error::other(e)))??;
        if object_count == 0 {
            for path in [&pack_path, &idx_path, &pack_path.with_extension("rev")] {
                let _ = tokio::fs::remove_file(path).await;
            }
            return Ok(None);
        }
        let pack_size = tokio::fs::metadata(&pack_path)
            .await
            .map_err(GitError::Io)?
            .len();
        let idx_size = tokio::fs::metadata(&idx_path)
            .await
            .map_err(GitError::Io)?
            .len();
        // Import validation uses supervised native Git. The normal publisher
        // refreshes gix after CAS; do not queue uncancellable index warm-up in
        // the acquisition deadline.
        Ok(Some(IngestedPack {
            checksum,
            pack_path,
            idx_path,
            pack_size,
            idx_size,
            object_count,
        }))
    }
}

#[cfg(all(test, unix))]
mod cancellation_tests {
    use super::*;
    use anyhow::{Result, ensure};
    use std::os::unix::fs::PermissionsExt;
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn import_index_timeout_kills_descendants_and_removes_scratch() -> Result<()> {
        let root = tempfile::tempdir()?;
        let repo = LocalRepo::init(
            root.path(),
            &crate::RepoId::new("test", "target")?,
            crate::ObjectFormat::Sha1,
        )?;
        let scratch = tempfile::Builder::new()
            .prefix("gitcask-import-index-")
            .tempdir_in(repo.path())?;
        let scratch_path = scratch.path().to_path_buf();
        let pid_file = root.path().join("pids");
        let script = root.path().join("blocked-index");
        std::fs::write(
            &script,
            r#"#!/bin/sh
sleep 300 &
child=$!
printf '%s %s\n' "$$" "$child" > "$1"
wait "$child"
"#,
        )?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))?;
        let mut command = tokio::process::Command::new(&script);
        command
            .arg(&pid_file)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let retained_path = scratch_path.clone();
        let work = tokio::spawn(async move {
            let processes = crate::ImportProcesses::default();
            let mut future = Box::pin(
                processes
                    .clone()
                    .scope(repo.finish_import_index(scratch, command)),
            );
            tokio::select! {
                _result=&mut future=>anyhow::bail!("blocked index unexpectedly completed"),
                ()=tokio::time::sleep(std::time::Duration::from_millis(500))=>{
                    ensure!(retained_path.exists(),"scratch disappeared before termination");
                    processes.terminate().await?;
                    ensure!(retained_path.exists(),"scratch removed before verified process termination");
                    drop(future);
                    Ok::<_,anyhow::Error>(())
                }
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !tokio::fs::try_exists(&pid_file).await? {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            Ok::<_, std::io::Error>(())
        })
        .await??;
        let pids = tokio::fs::read_to_string(pid_file).await?;
        work.await??;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let mut active = false;
                for pid in pids.split_whitespace() {
                    let output = tokio::process::Command::new("ps")
                        .args(["-p", pid, "-o", "stat="])
                        .output()
                        .await?;
                    let state = String::from_utf8_lossy(&output.stdout);
                    active |= !state.trim().is_empty() && !state.trim().starts_with('Z');
                }
                if !active && !tokio::fs::try_exists(&scratch_path).await? {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            Ok::<_, std::io::Error>(())
        })
        .await??;
        ensure!(!scratch_path.exists());
        Ok(())
    }
}
