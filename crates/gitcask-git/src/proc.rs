use std::io::Write;
use std::path::Path;
use std::process::Stdio;

use tokio::process::Command;

use crate::{GitError, LocalRepo};

impl LocalRepo {
    /// Run `git` with cwd=repo, `GIT_DIR` set, capturing output.
    pub async fn git(&self, args: &[&str]) -> Result<std::process::Output, GitError> {
        let mut cmd = Command::new("git");
        cmd.current_dir(&self.inner.path)
            .env("GIT_DIR", &self.inner.path)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.output().await.map_err(GitError::Io)
    }
    pub(crate) fn git_cmd_sync(&self, args: &[&str]) -> Result<std::process::Output, GitError> {
        let mut cmd = std::process::Command::new("git");
        cmd.current_dir(&self.inner.path)
            .env("GIT_DIR", &self.inner.path)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.output().map_err(GitError::Io)
    }

    pub(crate) async fn run_git_stdin(
        &self,
        cmd_name: &str,
        args: &[&str],
        stdin_bytes: &[u8],
    ) -> Result<std::process::Output, GitError> {
        let path = self.inner.path.clone();
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let stdin_bytes: Vec<u8> = stdin_bytes.to_vec();
        let cmd_name = cmd_name.to_string();
        let res = tokio::task::spawn_blocking(move || {
            let mut full_args: Vec<String> = Vec::with_capacity(args.len() + 1);
            full_args.push(cmd_name.clone());
            full_args.extend(args.iter().cloned());
            let mut cmd = std::process::Command::new("git");
            cmd.current_dir(&path)
                .env("GIT_DIR", &path)
                .args(&full_args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = cmd.spawn().map_err(GitError::Io)?;
            {
                let stdin = child.stdin.as_mut().unwrap();
                stdin.write_all(&stdin_bytes).map_err(GitError::Io)?;
            }
            child.wait_with_output().map_err(GitError::Io)
        })
        .await
        .map_err(|e| GitError::Io(std::io::Error::other(e)))??;
        Ok(res)
    }
}

pub(crate) fn rename_atomic(src: &Path, dst: &Path) -> Result<(), GitError> {
    match std::fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
            std::fs::copy(src, dst).map_err(GitError::Io)?;
            std::fs::remove_file(src).map_err(GitError::Io)?;
            Ok(())
        }
        Err(e) => Err(GitError::Io(e)),
    }
}
