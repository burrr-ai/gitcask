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
        for pid in &pids {
            let _ = tokio::process::Command::new("/bin/kill")
                .args(["-KILL", &format!("-{pid}")])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await?;
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
                let _ = tokio::process::Command::new("/bin/kill")
                    .args(["-KILL", &format!("-{pid}")])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await;
            });
        }
    }
}
