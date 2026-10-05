//! Import-owned process groups. Keep acquisition futures (and their scratch)
//! alive until every registered process has stopped before unwinding timeout.
use parking_lot::Mutex;
use std::{collections::HashSet, process::Stdio, sync::Arc};
#[derive(Clone, Default)]
pub struct ImportProcesses(Arc<Mutex<HashSet<u32>>>);
tokio::task_local! {static CURRENT:ImportProcesses;}
impl ImportProcesses {
    pub async fn scope<F: std::future::Future>(self, future: F) -> F::Output {
        CURRENT.scope(self, future).await
    }
    pub async fn terminate(&self) -> std::io::Result<()> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let pids: Vec<_> = self.0.lock().iter().copied().collect();
        let mut signal_failures = Vec::new();
        for pid in &pids {
            let status = kill_group(*pid).await?;
            if !status.success() {
                signal_failures.push(format!("group {pid}: {status}"));
            }
        }
        // Zombies cannot write/recreate scratch. Parent reaping happens when
        // the now-stopped child owners are subsequently dropped/polled.
        loop {
            let output = tokio::process::Command::new("ps")
                .args(["-eo", "pgid=,stat="])
                .output()
                .await?;
            if !output.status.success() {
                return Err(std::io::Error::other(
                    "cannot verify import process termination",
                ));
            }
            let text = String::from_utf8_lossy(&output.stdout);
            let active = text.lines().any(|line| {
                let mut fields = line.split_whitespace();
                let group = fields.next().and_then(|s| s.parse::<u32>().ok());
                let state = fields.next().unwrap_or_default();
                group.is_some_and(|group| pids.contains(&group)) && !state.starts_with('Z')
            });
            if !active {
                return Ok(());
            }
            // A group may have exited before kill (ESRCH), so a failed signal
            // is harmless only after the process table confirms no live members.
            if !signal_failures.is_empty() {
                return Err(std::io::Error::other(format!(
                    "import process groups remain active after failed kill: {}",
                    signal_failures.join(", ")
                )));
            }
            if std::time::Instant::now() >= deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "import process termination remains unconfirmed",
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}

async fn kill_group(pid: u32) -> std::io::Result<std::process::ExitStatus> {
    // procps parses a negative PGID as a signal option without `--`. The
    // delimiter is also supported by BSD kill; use the same argv in both paths.
    tokio::process::Command::new("/bin/kill")
        .args(["-KILL", "--", &format!("-{pid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
}
/// Per-child registration; normal completion removes it. Abnormal drop also
/// requests a kill as a backstop. Deadline cleanup uses terminate before drop.
pub struct ImportProcess(Option<u32>, Option<ImportProcesses>);
impl ImportProcess {
    pub fn new(pid: Option<u32>) -> Self {
        let scope = CURRENT.try_with(Clone::clone).ok();
        if let (Some(pid), Some(scope)) = (pid, &scope) {
            scope.0.lock().insert(pid);
        }
        Self(pid, scope)
    }
    pub fn finished(&mut self) {
        if let (Some(pid), Some(scope)) = (self.0.take(), &self.1) {
            scope.0.lock().remove(&pid);
        }
    }
}
impl Drop for ImportProcess {
    fn drop(&mut self) {
        if let Some(pid) = self.0.take() {
            if let Some(scope) = &self.1 {
                scope.0.lock().remove(&pid);
            }
            tokio::spawn(async move {
                match kill_group(pid).await {
                    Ok(status) if status.success() => {}
                    Ok(status) => tracing::warn!(pid, %status, "import process group kill failed"),
                    Err(error) => tracing::warn!(pid, %error, "import process group kill failed"),
                }
            });
        }
    }
}
