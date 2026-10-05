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
        let mut command = isolated_command(source);
        command.args(["pack-objects", "--stdout"]);
        if tips.is_some() {
            command.arg("--revs").stdin(Stdio::piped());
        } else {
            // Preserve the standalone CLI importer's Git progress output.
            command.arg("--all").stderr(Stdio::inherit());
        }
        let mut child = command.spawn().map_err(GitError::Io)?;
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
        let ((), pack) = tokio::try_join!(feed, self.ingest_pack(stdout, options))?;
        let status = child.wait().await.map_err(GitError::Io)?;
        if !status.success() {
            return Err(GitError::Subprocess {
                cmd: "git pack-objects import".into(),
                status: status.code(),
                stderr: "source closure unavailable".into(),
            });
        }
        Ok(pack)
    }
}
