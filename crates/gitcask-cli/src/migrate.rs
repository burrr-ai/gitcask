//! Resumable bulk migration from external git hosts.
//!
//! Host-specific listing and authentication live behind [`SourceAdapter`]. The
//! migration engine itself only knows how to clone a mirror, invoke the existing
//! import path, copy LFS objects, and persist local progress.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Read};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use futures::{StreamExt, stream};
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, HeaderValue, LINK};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use gitcask_config::Config;
use gitcask_git::RepoId;
use gitcask_proto::keys;
use gitcask_store::{
    DynStore, ObjectStore, Prefixed, PutBody, PutMode, PutOptions, StoreError, open_store,
};
use gitcask_wal::{Registry, WalError};

use crate::cli::parse_repo_id;

const GITEA_PAGE_SIZE: usize = 50;
const MAX_LFS_POINTER_BYTES: u64 = 1_024;
const STATE_VERSION: u32 = 1;

#[derive(clap::Args)]
pub(crate) struct GiteaArgs {
    /// Base URL of the Gitea instance.
    #[arg(long)]
    url: String,
    /// Gitea access token (or set `GITEA_TOKEN`).
    #[arg(long, env = "GITEA_TOKEN", hide_env_values = true)]
    token: String,
    /// Gitea organization or user whose repositories are migrated.
    #[arg(long)]
    owner: String,
    /// Migrate only this repository name (repeatable).
    #[arg(long = "repo")]
    repos: Vec<String>,
    /// Destination owner; defaults to the Gitea owner.
    #[arg(long)]
    to_owner: Option<String>,
    /// Repositories processed concurrently.
    #[arg(long, default_value = "2")]
    concurrency: NonZeroUsize,
    /// Local resumable state file.
    #[arg(long, default_value = "./gitcask-migrate-state.json")]
    state: PathBuf,
    /// List repositories and reported sizes without cloning or writing state.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Clone)]
struct MigrationOptions {
    owner: String,
    repos: Vec<String>,
    to_owner: String,
    concurrency: usize,
    state_path: PathBuf,
    dry_run: bool,
}

#[derive(Clone, Debug)]
struct SourceRepository {
    name: String,
    clone_url: String,
    size_kib: u64,
}

trait SourceAdapter: Sync {
    fn kind(&self) -> &'static str;
    fn identity(&self) -> &str;

    async fn list_repositories(&self, owner: &str) -> Result<Vec<SourceRepository>>;

    async fn clone_mirror(&self, repo: &SourceRepository, destination: &Path) -> Result<()>;

    async fn fetch_lfs(&self, git_dir: &Path) -> Result<()>;
}

struct GiteaSource {
    base_url: reqwest::Url,
    identity: String,
    token: String,
    authorization: HeaderValue,
    client: reqwest::Client,
    git_username: tokio::sync::OnceCell<String>,
}

#[derive(Deserialize)]
struct GiteaRepository {
    name: String,
    clone_url: String,
    #[serde(default)]
    size: u64,
}

#[derive(Deserialize)]
struct GiteaUser {
    login: String,
}

enum GiteaPage {
    NotFound,
    Repositories {
        repositories: Vec<GiteaRepository>,
        has_next: bool,
        total: Option<usize>,
    },
}

impl GiteaSource {
    fn new(url: &str, token: String) -> Result<Self> {
        let mut base_url = reqwest::Url::parse(url).context("invalid Gitea URL")?;
        anyhow::ensure!(
            matches!(base_url.scheme(), "http" | "https"),
            "Gitea URL must use http or https"
        );
        if !base_url.path().ends_with('/') {
            let path = format!("{}/", base_url.path());
            base_url.set_path(&path);
        }
        base_url.set_query(None);
        base_url.set_fragment(None);
        let identity = base_url.as_str().trim_end_matches('/').to_string();

        let mut authorization = HeaderValue::from_str(&format!("token {token}"))
            .context("Gitea token cannot be represented as an HTTP header")?;
        authorization.set_sensitive(true);
        let client = reqwest::Client::builder()
            .user_agent(concat!("gitcask/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_mins(1))
            .build()
            .context("building Gitea API client")?;

        Ok(Self {
            base_url,
            identity,
            token,
            authorization,
            client,
            git_username: tokio::sync::OnceCell::new(),
        })
    }

    fn endpoint(&self, owner_kind: &str, owner: &str) -> Result<reqwest::Url> {
        let mut endpoint = self
            .base_url
            .join("api/v1/")
            .context("building Gitea API URL")?;
        {
            let mut segments = endpoint
                .path_segments_mut()
                .map_err(|()| anyhow::anyhow!("Gitea URL cannot contain path segments"))?;
            segments.pop_if_empty();
            segments.extend([owner_kind, owner, "repos"]);
        }
        Ok(endpoint)
    }

    async fn fetch_page(&self, endpoint: &reqwest::Url, page: usize) -> Result<GiteaPage> {
        let response = self
            .client
            .get(endpoint.clone())
            .header(AUTHORIZATION, self.authorization.clone())
            .query(&[("page", page), ("limit", GITEA_PAGE_SIZE)])
            .send()
            .await
            .with_context(|| format!("requesting Gitea repository page {page}"))?;

        if response.status() == StatusCode::NOT_FOUND {
            return Ok(GiteaPage::NotFound);
        }
        if !response.status().is_success() {
            let status = response.status();
            let detail = response
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(500)
                .collect::<String>();
            bail!("Gitea API returned {status} for repository page {page}: {detail}");
        }

        let has_next = response
            .headers()
            .get(LINK)
            .and_then(|value| value.to_str().ok())
            .is_some_and(link_has_next);
        let total = response
            .headers()
            .get("x-total-count")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok());
        let repositories = response
            .json::<Vec<GiteaRepository>>()
            .await
            .with_context(|| format!("decoding Gitea repository page {page}"))?;
        Ok(GiteaPage::Repositories {
            repositories,
            has_next,
            total,
        })
    }

    async fn list_owner_kind(
        &self,
        owner_kind: &str,
        owner: &str,
    ) -> Result<Option<Vec<SourceRepository>>> {
        let endpoint = self.endpoint(owner_kind, owner)?;
        let mut page = 1usize;
        let mut listed = Vec::new();
        loop {
            let (repositories, has_next, total) = match self.fetch_page(&endpoint, page).await? {
                GiteaPage::NotFound if page == 1 => return Ok(None),
                GiteaPage::NotFound => {
                    bail!("Gitea repository page {page} disappeared during pagination")
                }
                GiteaPage::Repositories {
                    repositories,
                    has_next,
                    total,
                } => (repositories, has_next, total),
            };
            let count = repositories.len();
            listed.extend(repositories.into_iter().map(|repo| SourceRepository {
                name: repo.name,
                clone_url: repo.clone_url,
                size_kib: repo.size,
            }));

            let total_has_more = total.is_some_and(|total| listed.len() < total);
            if count == 0 || (!has_next && !total_has_more && count < GITEA_PAGE_SIZE) {
                break;
            }
            page = page.checked_add(1).context("Gitea pagination overflow")?;
        }
        Ok(Some(listed))
    }

    async fn authenticated_user(&self) -> Result<&str> {
        let username = self
            .git_username
            .get_or_try_init(|| async {
                let endpoint = self
                    .base_url
                    .join("api/v1/user")
                    .context("building Gitea current-user URL")?;
                let response = self
                    .client
                    .get(endpoint)
                    .header(AUTHORIZATION, self.authorization.clone())
                    .send()
                    .await
                    .context("requesting the Gitea token owner")?;
                if !response.status().is_success() {
                    let status = response.status();
                    let detail = response
                        .text()
                        .await
                        .unwrap_or_default()
                        .chars()
                        .take(500)
                        .collect::<String>();
                    bail!("Gitea API returned {status} for the token owner: {detail}");
                }
                let user = response
                    .json::<GiteaUser>()
                    .await
                    .context("decoding the Gitea token owner")?;
                anyhow::ensure!(
                    !user.login.is_empty()
                        && !user.login.contains(['\r', '\n'])
                        && !user.login.contains('='),
                    "Gitea returned an invalid token-owner login"
                );
                Ok::<_, anyhow::Error>(user.login)
            })
            .await?;
        Ok(username)
    }

    fn configure_git_auth(&self, command: &mut tokio::process::Command, username: &str) {
        // Gitea access tokens are Git HTTPS passwords. The constant helper
        // reads both values from the child environment, keeping them out of
        // the clone URL and argv.
        const HELPER: &str = "!f() { printf '%s\\n' \"username=$GITCASK_GITEA_USERNAME\" \"password=$GITCASK_GITEA_TOKEN\"; }; f";
        command
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "credential.helper")
            .env("GIT_CONFIG_VALUE_0", HELPER)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GITCASK_GITEA_USERNAME", username)
            .env("GITCASK_GITEA_TOKEN", &self.token);
    }
}

impl SourceAdapter for GiteaSource {
    fn kind(&self) -> &'static str {
        "gitea"
    }

    fn identity(&self) -> &str {
        &self.identity
    }

    async fn list_repositories(&self, owner: &str) -> Result<Vec<SourceRepository>> {
        if let Some(repositories) = self.list_owner_kind("orgs", owner).await? {
            return normalize_repositories(repositories);
        }
        if let Some(repositories) = self.list_owner_kind("users", owner).await? {
            return normalize_repositories(repositories);
        }
        bail!("Gitea owner `{owner}` was not found as an organization or user")
    }

    async fn clone_mirror(&self, repo: &SourceRepository, destination: &Path) -> Result<()> {
        let username = self.authenticated_user().await?;
        let mut command = tokio::process::Command::new("git");
        command
            .args(["clone", "--mirror", "--"])
            .arg(&repo.clone_url)
            .arg(destination);
        self.configure_git_auth(&mut command, username);
        run_git(command, "git clone --mirror").await
    }

    async fn fetch_lfs(&self, git_dir: &Path) -> Result<()> {
        let username = self.authenticated_user().await?;
        let mut command = tokio::process::Command::new("git");
        command
            .args(["lfs", "fetch", "--all", "origin"])
            .current_dir(git_dir);
        self.configure_git_auth(&mut command, username);
        run_git(command, "git lfs fetch --all").await
    }
}

fn link_has_next(link: &str) -> bool {
    link.split(',').any(|part| {
        let relation = part.split(';').skip(1).map(str::trim);
        relation.into_iter().any(|value| {
            value.eq_ignore_ascii_case("rel=\"next\"") || value.eq_ignore_ascii_case("rel=next")
        })
    })
}

fn normalize_repositories(repositories: Vec<SourceRepository>) -> Result<Vec<SourceRepository>> {
    let mut by_name = BTreeMap::new();
    for repository in repositories {
        anyhow::ensure!(
            !repository.name.is_empty(),
            "Gitea returned a nameless repository"
        );
        anyhow::ensure!(
            !repository.clone_url.is_empty(),
            "Gitea returned no clone URL for `{}`",
            repository.name
        );
        if by_name
            .insert(repository.name.clone(), repository)
            .is_some()
        {
            bail!("Gitea returned a duplicate repository name")
        }
    }
    Ok(by_name.into_values().collect())
}

pub(crate) async fn run_gitea(args: GiteaArgs, cfg: &Arc<Config>) -> Result<()> {
    let GiteaArgs {
        url,
        token,
        owner,
        repos,
        to_owner,
        concurrency,
        state,
        dry_run,
    } = args;
    let source = GiteaSource::new(&url, token)?;
    let options = MigrationOptions {
        to_owner: to_owner.unwrap_or_else(|| owner.clone()),
        owner,
        repos,
        concurrency: concurrency.get(),
        state_path: state,
        dry_run,
    };
    run_with_adapter(&source, &options, cfg, None).await
}

async fn run_with_adapter<A: SourceAdapter>(
    adapter: &A,
    options: &MigrationOptions,
    cfg: &Arc<Config>,
    supplied_store: Option<DynStore>,
) -> Result<()> {
    validate_owner(&options.to_owner)?;
    println!(
        "listing {} repositories for `{}` from {}",
        adapter.kind(),
        options.owner,
        adapter.identity()
    );
    let repositories = adapter.list_repositories(&options.owner).await?;
    let selected = select_repositories(repositories, &options.repos, &options.to_owner)?;
    print_plan(&selected, &options.to_owner, options.dry_run);
    if options.dry_run {
        println!(
            "dry-run complete: {} repositories, no clones, state, or gitcask writes",
            selected.len()
        );
        return Ok(());
    }

    let store = match supplied_store {
        Some(store) => store,
        None => open_store(cfg).await?,
    };
    tokio::fs::create_dir_all(&cfg.cache.dir)
        .await
        .with_context(|| format!("creating cache directory {}", cfg.cache.dir.display()))?;
    let registry = Registry::new(store, cfg.clone());
    migrate_selected(adapter, options, selected, cfg, &registry).await
}

fn validate_owner(owner: &str) -> Result<()> {
    parse_repo_id(&format!("{owner}/validation"))?;
    Ok(())
}

fn select_repositories(
    repositories: Vec<SourceRepository>,
    requested: &[String],
    to_owner: &str,
) -> Result<Vec<SourceRepository>> {
    let mut available = repositories
        .into_iter()
        .map(|repository| (repository.name.clone(), repository))
        .collect::<BTreeMap<_, _>>();
    let selected = if requested.is_empty() {
        available.into_values().collect::<Vec<_>>()
    } else {
        let requested = requested.iter().cloned().collect::<BTreeSet<_>>();
        let missing = requested
            .iter()
            .filter(|name| !available.contains_key(*name))
            .cloned()
            .collect::<Vec<_>>();
        anyhow::ensure!(
            missing.is_empty(),
            "requested repositories were not returned by Gitea: {}",
            missing.join(", ")
        );
        requested
            .into_iter()
            .filter_map(|name| available.remove(&name))
            .collect::<Vec<_>>()
    };

    for repository in &selected {
        parse_repo_id(&format!("{to_owner}/{}", repository.name)).with_context(|| {
            format!(
                "Gitea repository name `{}` is not a valid gitcask name",
                repository.name
            )
        })?;
    }
    Ok(selected)
}

fn print_plan(repositories: &[SourceRepository], to_owner: &str, dry_run: bool) {
    let total_kib = repositories.iter().fold(0u64, |sum, repository| {
        sum.saturating_add(repository.size_kib)
    });
    let label = if dry_run { "dry-run" } else { "plan" };
    println!(
        "{label}: {} repositories, reported git size {}",
        repositories.len(),
        format_kib(total_kib)
    );
    for repository in repositories {
        println!(
            "  {}/{}  {}",
            to_owner,
            repository.name,
            format_kib(repository.size_kib)
        );
    }
}

fn format_kib(kib: u64) -> String {
    const KIB_PER_MIB: u64 = 1_024;
    const KIB_PER_GIB: u64 = KIB_PER_MIB * 1_024;
    if kib >= KIB_PER_GIB {
        format_one_decimal(kib, KIB_PER_GIB, "GiB")
    } else if kib >= KIB_PER_MIB {
        format_one_decimal(kib, KIB_PER_MIB, "MiB")
    } else {
        format!("{kib} KiB")
    }
}

fn format_one_decimal(value: u64, unit: u64, suffix: &str) -> String {
    let whole = value / unit;
    let tenth = value % unit * 10 / unit;
    format!("{whole}.{tenth} {suffix}")
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct StateIdentity {
    source: String,
    source_url: String,
    source_owner: String,
    target_owner: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CompletedRepository {
    target: String,
    source_size_kib: u64,
    lfs_objects: usize,
    completed_at_unix_seconds: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct MigrationState {
    version: u32,
    identity: StateIdentity,
    completed: BTreeMap<String, CompletedRepository>,
}

impl MigrationState {
    fn new(identity: StateIdentity) -> Self {
        Self {
            version: STATE_VERSION,
            identity,
            completed: BTreeMap::new(),
        }
    }
}

async fn load_state(path: &Path, identity: StateIdentity) -> Result<MigrationState> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(MigrationState::new(identity));
        }
        Err(error) => {
            return Err(error).with_context(|| format!("reading state file {}", path.display()));
        }
    };
    let state: MigrationState = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing state file {}", path.display()))?;
    anyhow::ensure!(
        state.version == STATE_VERSION,
        "state file {} has unsupported version {}",
        path.display(),
        state.version
    );
    anyhow::ensure!(
        state.identity == identity,
        "state file {} belongs to a different migration; use another --state path",
        path.display()
    );
    Ok(state)
}

async fn write_state(path: &Path, state: &MigrationState) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating state directory {}", parent.display()))?;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("state path must have a UTF-8 file name")?;
    let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    let mut json = serde_json::to_vec_pretty(state).context("serializing migration state")?;
    json.push(b'\n');
    tokio::fs::write(&temporary, json)
        .await
        .with_context(|| format!("writing state file {}", temporary.display()))?;
    tokio::fs::rename(&temporary, path)
        .await
        .with_context(|| format!("installing state file {}", path.display()))?;
    Ok(())
}

struct MigrationJob {
    index: usize,
    total: usize,
    repository: SourceRepository,
    target: String,
}

struct MigrationSuccess {
    lfs_objects: usize,
    git_already_present: bool,
}

async fn migrate_selected<A: SourceAdapter>(
    adapter: &A,
    options: &MigrationOptions,
    selected: Vec<SourceRepository>,
    cfg: &Arc<Config>,
    registry: &Arc<Registry>,
) -> Result<()> {
    let identity = StateIdentity {
        source: adapter.kind().to_string(),
        source_url: adapter.identity().to_string(),
        source_owner: options.owner.clone(),
        target_owner: options.to_owner.clone(),
    };
    let mut state = load_state(&options.state_path, identity).await?;
    let total = selected.len();
    let mut skipped = 0usize;
    let mut jobs = Vec::new();
    for (offset, repository) in selected.into_iter().enumerate() {
        let index = offset + 1;
        let target = format!("{}/{}", options.to_owner, repository.name);
        if state.completed.contains_key(&repository.name) {
            skipped += 1;
            println!("[{index}/{total}] {target}: already complete, skipping");
        } else {
            jobs.push(MigrationJob {
                index,
                total,
                repository,
                target,
            });
        }
    }

    let mut running = stream::iter(jobs.into_iter().map(|job| async move {
        println!(
            "[{}/{}] {}: cloning {}",
            job.index, job.total, job.target, job.repository.name
        );
        let result = migrate_one(adapter, &job.repository, &job.target, cfg, registry).await;
        (job, result)
    }))
    .buffer_unordered(options.concurrency);

    let mut succeeded = 0usize;
    let mut failures = Vec::new();
    while let Some((job, result)) = running.next().await {
        match result {
            Ok(success) => {
                let mut updated = state.clone();
                updated.completed.insert(
                    job.repository.name.clone(),
                    CompletedRepository {
                        target: job.target.clone(),
                        source_size_kib: job.repository.size_kib,
                        lfs_objects: success.lfs_objects,
                        completed_at_unix_seconds: unix_now(),
                    },
                );
                match write_state(&options.state_path, &updated).await {
                    Ok(()) => {
                        state = updated;
                        succeeded += 1;
                        let git_status = if success.git_already_present {
                            "git already present"
                        } else {
                            "git imported"
                        };
                        println!(
                            "[{}/{}] {}: complete ({git_status}, {} LFS objects)",
                            job.index, job.total, job.target, success.lfs_objects
                        );
                    }
                    Err(error) => {
                        failures.push((job.target, format!("saving progress: {error:#}")));
                    }
                }
            }
            Err(error) => failures.push((job.target, format!("{error:#}"))),
        }
    }

    println!(
        "migration summary: {succeeded} completed, {skipped} skipped, {} failed",
        failures.len()
    );
    if !failures.is_empty() {
        eprintln!("failed repositories:");
        for (target, reason) in &failures {
            eprintln!("  {target}: {reason}");
        }
        bail!("{} repository migration(s) failed", failures.len());
    }
    Ok(())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

async fn migrate_one<A: SourceAdapter>(
    adapter: &A,
    repository: &SourceRepository,
    target: &str,
    cfg: &Arc<Config>,
    registry: &Arc<Registry>,
) -> Result<MigrationSuccess> {
    let temporary = tokio::task::spawn_blocking(|| {
        tempfile::Builder::new()
            .prefix("gitcask-migrate-")
            .tempdir()
    })
    .await
    .context("joining temporary-directory task")?
    .context("creating temporary migration directory")?;
    let mirror = temporary.path().join("mirror.git");
    let result = migrate_one_in(adapter, repository, target, cfg, registry, &mirror).await;

    let temporary_path = temporary.keep();
    let cleanup = tokio::fs::remove_dir_all(&temporary_path)
        .await
        .with_context(|| format!("removing temporary directory {}", temporary_path.display()));
    match (result, cleanup) {
        (Ok(success), Ok(())) => Ok(success),
        (Ok(_), Err(cleanup_error)) => Err(cleanup_error),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup_error)) => Err(error.context(format!(
            "also failed to clean {}: {cleanup_error:#}",
            temporary_path.display()
        ))),
    }
}

async fn migrate_one_in<A: SourceAdapter>(
    adapter: &A,
    repository: &SourceRepository,
    target: &str,
    cfg: &Arc<Config>,
    registry: &Arc<Registry>,
    mirror: &Path,
) -> Result<MigrationSuccess> {
    let (owner, name) = parse_repo_id(target)?;
    let id = RepoId::new(owner, name)?;
    let git_already_present = match registry.open(&id).await {
        Ok(handle) => handle.manifest().head_seq > 0,
        Err(WalError::NotFound) => false,
        Err(error) => return Err(error).context("checking destination repository"),
    };

    adapter
        .clone_mirror(repository, mirror)
        .await
        .with_context(|| format!("cloning `{}`", repository.name))?;

    if git_already_present {
        println!("  {target}: destination has committed Git data; import skipped");
    } else {
        crate::import::run_with_registry(
            mirror.to_path_buf(),
            target.to_string(),
            false,
            Vec::new(),
            cfg,
            registry,
        )
        .await
        .with_context(|| format!("importing `{target}` through the WAL publish path"))?;
    }

    println!("  {target}: scanning history for LFS pointers");
    let pointers = find_lfs_pointers(mirror).await?;
    if !pointers.is_empty() {
        println!("  {target}: fetching {} LFS objects", pointers.len());
        adapter
            .fetch_lfs(mirror)
            .await
            .with_context(|| format!("fetching LFS objects for `{}`", repository.name))?;
        upload_lfs(registry, &id, mirror, &pointers).await?;
    }

    Ok(MigrationSuccess {
        lfs_objects: pointers.len(),
        git_already_present,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LfsPointer {
    oid: String,
    size: u64,
}

async fn find_lfs_pointers(git_dir: &Path) -> Result<BTreeMap<String, LfsPointer>> {
    let git_dir = git_dir.to_path_buf();
    tokio::task::spawn_blocking(move || find_lfs_pointers_blocking(&git_dir))
        .await
        .context("joining LFS pointer scan")?
}

fn find_lfs_pointers_blocking(git_dir: &Path) -> Result<BTreeMap<String, LfsPointer>> {
    let mut revisions = std::process::Command::new("git")
        .args(["rev-list", "--objects", "--all"])
        .current_dir(git_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning git rev-list for LFS pointer scan")?;
    let revisions_stdout = revisions
        .stdout
        .take()
        .context("git rev-list has no stdout")?;
    let mut check = std::process::Command::new("git")
        .args([
            "cat-file",
            "--batch-check=%(objectname) %(objecttype) %(objectsize) %(rest)",
        ])
        .current_dir(git_dir)
        .stdin(Stdio::from(revisions_stdout))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning git cat-file for LFS pointer scan")?;
    let check_stdout = check.stdout.take().context("git cat-file has no stdout")?;
    let mut pointers = BTreeMap::new();
    for line in std::io::BufReader::new(check_stdout).lines() {
        let line = line.context("reading git cat-file output")?;
        let mut fields = line.split_whitespace();
        let object = fields.next().context("cat-file omitted object id")?;
        let kind = fields.next().context("cat-file omitted object type")?;
        let size = fields
            .next()
            .context("cat-file omitted object size")?
            .parse::<u64>()
            .context("cat-file returned an invalid object size")?;
        if kind != "blob" || size > MAX_LFS_POINTER_BYTES {
            continue;
        }
        let output = std::process::Command::new("git")
            .args(["cat-file", "blob", object])
            .current_dir(git_dir)
            .output()
            .with_context(|| format!("reading candidate LFS pointer {object}"))?;
        anyhow::ensure!(
            output.status.success(),
            "git cat-file blob failed for {object}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if let Some(pointer) = parse_lfs_pointer(&output.stdout)
            && let Some(previous) = pointers.insert(pointer.oid.clone(), pointer.clone())
        {
            anyhow::ensure!(
                previous.size == pointer.size,
                "LFS pointer {} has conflicting sizes",
                pointer.oid
            );
        }
    }

    let check_output = check
        .wait_with_output()
        .context("waiting for git cat-file pointer scan")?;
    anyhow::ensure!(
        check_output.status.success(),
        "git cat-file pointer scan failed: {}",
        String::from_utf8_lossy(&check_output.stderr)
    );
    let revisions_output = revisions
        .wait_with_output()
        .context("waiting for git rev-list pointer scan")?;
    anyhow::ensure!(
        revisions_output.status.success(),
        "git rev-list pointer scan failed: {}",
        String::from_utf8_lossy(&revisions_output.stderr)
    );
    Ok(pointers)
}

fn parse_lfs_pointer(bytes: &[u8]) -> Option<LfsPointer> {
    if u64::try_from(bytes.len()).ok()? > MAX_LFS_POINTER_BYTES {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    if text.lines().next()? != "version https://git-lfs.github.com/spec/v1" {
        return None;
    }
    let oid = text
        .lines()
        .find_map(|line| line.strip_prefix("oid sha256:"))?;
    if !keys::lfs_oid_ok(oid) {
        return None;
    }
    let size = text
        .lines()
        .find_map(|line| line.strip_prefix("size "))?
        .parse::<u64>()
        .ok()?;
    Some(LfsPointer {
        oid: oid.to_ascii_lowercase(),
        size,
    })
}

async fn upload_lfs(
    registry: &Registry,
    id: &RepoId,
    git_dir: &Path,
    pointers: &BTreeMap<String, LfsPointer>,
) -> Result<()> {
    let store = Prefixed::new(registry.store().clone(), id.store_prefix());
    let total = pointers.len();
    for (offset, pointer) in pointers.values().enumerate() {
        anyhow::ensure!(
            pointer.size <= registry.config().lfs.max_object_bytes.as_u64(),
            "LFS object {} is {} bytes, above lfs.max_object_bytes ({})",
            pointer.oid,
            pointer.size,
            registry.config().lfs.max_object_bytes
        );
        let first = pointer
            .oid
            .get(..2)
            .context("validated LFS oid has no first path segment")?;
        let second = pointer
            .oid
            .get(2..4)
            .context("validated LFS oid has no second path segment")?;
        let path = git_dir
            .join("lfs")
            .join("objects")
            .join(first)
            .join(second)
            .join(&pointer.oid);
        let verify_path = path.clone();
        let verify_pointer = pointer.clone();
        tokio::task::spawn_blocking(move || verify_lfs_object(&verify_path, &verify_pointer))
            .await
            .context("joining LFS verification task")??;
        let key = keys::lfs_key(&pointer.oid);
        let result = store
            .put(
                &key,
                PutBody::File(path),
                PutOptions {
                    mode: PutMode::Create,
                    content_type: Some("application/octet-stream"),
                    immutable: true,
                },
            )
            .await;
        match result {
            Ok(_) | Err(StoreError::PreconditionFailed { .. }) => {
                println!("    LFS {}/{}: {}", offset + 1, total, pointer.oid);
            }
            Err(error) => {
                return Err(error).context(format!("uploading LFS object {}", pointer.oid));
            }
        }
    }
    Ok(())
}

fn verify_lfs_object(path: &Path, pointer: &LfsPointer) -> Result<()> {
    let file = std::fs::File::open(path).with_context(|| {
        format!(
            "LFS object {} was not fetched into {}",
            pointer.oid,
            path.display()
        )
    })?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.len() == pointer.size,
        "LFS object {} has size {}, expected {}",
        pointer.oid,
        metadata.len(),
        pointer.size
    );
    let mut reader = std::io::BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8 * 1_024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(
            buffer
                .get(..read)
                .context("file read exceeded the hash buffer")?,
        );
    }
    let actual = format!("{:x}", hasher.finalize());
    anyhow::ensure!(
        actual == pointer.oid,
        "LFS object {} failed sha256 verification (got {actual})",
        pointer.oid
    );
    Ok(())
}

async fn run_git(mut command: tokio::process::Command, operation: &str) -> Result<()> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .with_context(|| format!("spawning {operation}"))?;
    let status = child
        .wait()
        .await
        .with_context(|| format!("waiting for {operation}"))?;
    anyhow::ensure!(status.success(), "{operation} failed with {status}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::extract::{Path as AxumPath, Query, State};
    use axum::http::{HeaderMap, StatusCode as AxumStatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use axum::{Json, Router};
    use gitcask_config::StoreBackend;
    use gitcask_store::ObjectStoreExt;
    use gitcask_store::memory::MemoryStore;
    use serde_json::json;

    use super::*;

    struct FakeSource {
        identity: String,
        repositories: Vec<SourceRepository>,
        clone_calls: AtomicUsize,
    }

    impl FakeSource {
        fn new(identity: &str, repositories: Vec<SourceRepository>) -> Self {
            Self {
                identity: identity.to_string(),
                repositories,
                clone_calls: AtomicUsize::new(0),
            }
        }
    }

    impl SourceAdapter for FakeSource {
        fn kind(&self) -> &'static str {
            "fake"
        }

        fn identity(&self) -> &str {
            &self.identity
        }

        async fn list_repositories(&self, _owner: &str) -> Result<Vec<SourceRepository>> {
            Ok(self.repositories.clone())
        }

        async fn clone_mirror(&self, repo: &SourceRepository, destination: &Path) -> Result<()> {
            self.clone_calls.fetch_add(1, Ordering::SeqCst);
            let mut command = tokio::process::Command::new("git");
            command
                .args(["clone", "--quiet", "--mirror", "--"])
                .arg(&repo.clone_url)
                .arg(destination);
            run_git(command, "test git clone --mirror").await
        }

        async fn fetch_lfs(&self, _git_dir: &Path) -> Result<()> {
            bail!("test repository unexpectedly contained an LFS pointer")
        }
    }

    fn git(dir: &Path, args: &[&str]) -> Result<String> {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .with_context(|| format!("git {}", args.join(" ")))?;
        anyhow::ensure!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn source_repo(root: &Path, name: &str, contents: &str) -> Result<SourceRepository> {
        let path = root.join(name);
        let path_arg = path.to_str().context("test path is not UTF-8")?;
        git(root, &["init", "-q", "-b", "main", path_arg])?;
        std::fs::write(path.join("state.txt"), contents)?;
        git(&path, &["add", "state.txt"])?;
        git(
            &path,
            &[
                "-c",
                "user.name=gitcask test",
                "-c",
                "user.email=gitcask@example.com",
                "commit",
                "-q",
                "-m",
                contents,
            ],
        )?;
        Ok(SourceRepository {
            name: name.to_string(),
            clone_url: path_arg.to_string(),
            size_kib: 1,
        })
    }

    fn test_config(root: &Path) -> Arc<Config> {
        let mut cfg = Config::default();
        cfg.store.backend = StoreBackend::Memory;
        cfg.store.bucket = "migration-test".into();
        cfg.cache.dir = root.join("cache");
        cfg.wal.freshness_ttl = Duration::ZERO;
        Arc::new(cfg)
    }

    fn options(root: &Path, dry_run: bool) -> MigrationOptions {
        MigrationOptions {
            owner: "source".into(),
            repos: Vec::new(),
            to_owner: "target".into(),
            concurrency: 2,
            state_path: root.join("state.json"),
            dry_run,
        }
    }

    #[test]
    fn parses_lfs_pointer_and_link_pagination() {
        let oid = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let pointer = parse_lfs_pointer(
            format!("version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize 42\n")
                .as_bytes(),
        )
        .expect("valid pointer");
        assert_eq!(pointer.oid, oid);
        assert_eq!(pointer.size, 42);
        assert!(link_has_next(
            "</api/v1/orgs/o/repos?limit=1&page=2>; rel=\"next\", </last>; rel=\"last\""
        ));
        assert!(!link_has_next("</last>; rel=\"last\""));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn dry_run_does_not_clone_write_state_or_touch_store() -> Result<()> {
        let root = tempfile::tempdir()?;
        let source = FakeSource::new(
            "fake://dry-run",
            vec![SourceRepository {
                name: "only".into(),
                clone_url: root.path().join("does-not-exist").display().to_string(),
                size_kib: 17,
            }],
        );
        let cfg = test_config(root.path());
        let memory = MemoryStore::shared();
        let store: DynStore = memory;
        let migration_options = options(root.path(), true);

        run_with_adapter(&source, &migration_options, &cfg, Some(store.clone())).await?;

        assert_eq!(source.clone_calls.load(Ordering::SeqCst), 0);
        assert!(!migration_options.state_path.exists());
        let mut objects = store.list("", None);
        assert!(objects.next().await.is_none());
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn scans_verifies_and_uploads_lfs_objects() -> Result<()> {
        let root = tempfile::tempdir()?;
        let contents = b"large file contents\n";
        let oid = format!("{:x}", Sha256::digest(contents));
        let pointer_text = format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {}\n",
            contents.len()
        );
        let repository = source_repo(root.path(), "lfs-source", &pointer_text)?;
        let git_dir = Path::new(&repository.clone_url).join(".git");
        let pointers = find_lfs_pointers(&git_dir).await?;
        assert_eq!(pointers.len(), 1);
        assert_eq!(pointers.get(&oid).map(|pointer| pointer.size), Some(20));

        let object_path = git_dir
            .join("lfs/objects")
            .join(oid.get(..2).context("oid prefix")?)
            .join(oid.get(2..4).context("oid second prefix")?)
            .join(&oid);
        tokio::fs::create_dir_all(object_path.parent().context("LFS object parent")?).await?;
        tokio::fs::write(&object_path, contents).await?;

        let cfg = test_config(root.path());
        let memory = MemoryStore::shared();
        let store: DynStore = memory;
        let registry = Registry::new(store.clone(), cfg);
        let id = RepoId::new("target", "lfs")?;
        upload_lfs(&registry, &id, &git_dir, &pointers).await?;

        let prefixed = Prefixed::new(store, id.store_prefix());
        let (_, uploaded) = prefixed
            .get_bytes(&keys::lfs_key(&oid))
            .await?
            .context("uploaded LFS object is missing")?;
        assert_eq!(uploaded.as_ref(), contents);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn resumes_after_one_completed_repository() -> Result<()> {
        let root = tempfile::tempdir()?;
        let alpha = source_repo(root.path(), "alpha", "alpha")?;
        let beta = source_repo(root.path(), "beta", "beta")?;
        let cfg = test_config(root.path());
        let memory = MemoryStore::shared();
        let store: DynStore = memory;
        let migration_options = options(root.path(), false);

        let first = FakeSource::new("fake://resume", vec![alpha.clone()]);
        run_with_adapter(&first, &migration_options, &cfg, Some(store.clone())).await?;
        assert_eq!(first.clone_calls.load(Ordering::SeqCst), 1);

        let mut invalid_alpha = alpha;
        invalid_alpha.clone_url = root.path().join("removed-alpha").display().to_string();
        let resumed = FakeSource::new("fake://resume", vec![invalid_alpha, beta]);
        run_with_adapter(&resumed, &migration_options, &cfg, Some(store.clone())).await?;
        assert_eq!(resumed.clone_calls.load(Ordering::SeqCst), 1);

        let state: MigrationState =
            serde_json::from_slice(&tokio::fs::read(&migration_options.state_path).await?)?;
        assert_eq!(state.completed.len(), 2);
        assert!(state.completed.contains_key("alpha"));
        assert!(state.completed.contains_key("beta"));
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn repository_failure_does_not_stop_other_migrations() -> Result<()> {
        let root = tempfile::tempdir()?;
        let good = source_repo(root.path(), "good", "good")?;
        let bad = SourceRepository {
            name: "bad".into(),
            clone_url: root.path().join("missing").display().to_string(),
            size_kib: 1,
        };
        let source = FakeSource::new("fake://partial-failure", vec![bad, good]);
        let cfg = test_config(root.path());
        let memory = MemoryStore::shared();
        let store: DynStore = memory;
        let migration_options = options(root.path(), false);

        let error = run_with_adapter(&source, &migration_options, &cfg, Some(store))
            .await
            .expect_err("one failed repository must make the command fail");
        assert!(error.to_string().contains("1 repository migration"));

        let state: MigrationState =
            serde_json::from_slice(&tokio::fs::read(&migration_options.state_path).await?)?;
        assert_eq!(state.completed.len(), 1);
        assert!(state.completed.contains_key("good"));
        assert!(!state.completed.contains_key("bad"));
        Ok(())
    }

    #[derive(Clone)]
    struct GiteaStubState {
        repositories: Arc<Vec<serde_json::Value>>,
        requests: Arc<AtomicUsize>,
    }

    async fn gitea_repositories(
        State(state): State<GiteaStubState>,
        AxumPath(owner): AxumPath<String>,
        Query(query): Query<HashMap<String, usize>>,
        headers: HeaderMap,
    ) -> Response {
        if owner != "source"
            || headers
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                != Some("token test-token")
        {
            return AxumStatusCode::UNAUTHORIZED.into_response();
        }
        state.requests.fetch_add(1, Ordering::SeqCst);
        let page = query.get("page").copied().unwrap_or(1);
        let Some(repository) = state.repositories.get(page.saturating_sub(1)).cloned() else {
            return Json(Vec::<serde_json::Value>::new()).into_response();
        };
        let mut response = Json(vec![repository]).into_response();
        response
            .headers_mut()
            .insert("x-total-count", HeaderValue::from_static("2"));
        if page == 1 {
            response.headers_mut().insert(
                LINK,
                HeaderValue::from_static(
                    "</api/v1/orgs/source/repos?page=2&limit=50>; rel=\"next\"",
                ),
            );
        }
        response
    }

    async fn gitea_user(headers: HeaderMap) -> Response {
        if headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            == Some("token test-token")
        {
            Json(json!({"login": "token-owner"})).into_response()
        } else {
            AxumStatusCode::UNAUTHORIZED.into_response()
        }
    }

    fn unused_loopback_address() -> Result<std::net::SocketAddr> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        Ok(listener.local_addr()?)
    }

    async fn wait_until_listening(address: std::net::SocketAddr) -> Result<()> {
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(address).await.is_ok() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        bail!("server did not listen on {address}")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn gitea_two_page_migration_clones_through_gitcask() -> Result<()> {
        let root = tempfile::tempdir()?;
        let alpha = source_repo(root.path(), "stub-alpha", "from alpha")?;
        let beta = source_repo(root.path(), "stub-beta", "from beta")?;
        let requests = Arc::new(AtomicUsize::new(0));
        let stub_state = GiteaStubState {
            repositories: Arc::new(vec![
                json!({"name": alpha.name, "clone_url": alpha.clone_url, "size": 1}),
                json!({"name": beta.name, "clone_url": beta.clone_url, "size": 2}),
            ]),
            requests: requests.clone(),
        };
        let app = Router::new()
            .route("/api/v1/orgs/{owner}/repos", get(gitea_repositories))
            .route("/api/v1/user", get(gitea_user))
            .with_state(stub_state);
        let api_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let api_address = api_listener.local_addr()?;
        let api_task = tokio::spawn(async move { axum::serve(api_listener, app).await });

        let source = GiteaSource::new(&format!("http://{api_address}"), "test-token".into())?;
        let cfg = test_config(root.path());
        let memory = MemoryStore::shared();
        let store: DynStore = memory;
        let migration_options = options(root.path(), false);
        run_with_adapter(&source, &migration_options, &cfg, Some(store.clone())).await?;
        assert_eq!(requests.load(Ordering::SeqCst), 2);

        let server_address = unused_loopback_address()?;
        let mut server_cfg = (*cfg).clone();
        server_cfg.server.listen = server_address;
        server_cfg.cache.dir = root.path().join("server-cache");
        server_cfg.validate()?;
        let app_state = gitcask_server::AppState::new(Arc::new(server_cfg), store).await?;
        let server_task = tokio::spawn(gitcask_server::serve(
            app_state,
            std::future::pending::<()>(),
        ));
        wait_until_listening(server_address).await?;

        for repository in [&alpha, &beta] {
            let clone = root.path().join(format!("clone-{}", repository.name));
            let clone_arg = clone.to_str().context("clone path is not UTF-8")?;
            let remote = format!("http://{server_address}/target/{}.git", repository.name);
            git(root.path(), &["clone", "-q", &remote, clone_arg])?;
            let source_head = git(Path::new(&repository.clone_url), &["rev-parse", "HEAD"])?;
            assert_eq!(git(&clone, &["rev-parse", "HEAD"])?, source_head);
            assert_eq!(
                std::fs::read_to_string(clone.join("state.txt"))?,
                format!("from {}", repository.name.trim_start_matches("stub-"))
            );
        }

        server_task.abort();
        let _ = server_task.await;
        api_task.abort();
        let _ = api_task.await;
        Ok(())
    }
}
