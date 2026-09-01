//! Git subprocess execution and parsing for the browsing API.

use std::collections::HashMap;

use crate::error::ApiError;

use super::{Commit, CompareFile, Stat};

const MAX_PATCH: usize = 2 * 1024 * 1024;

/// Meaning of a nonzero exit for one explicitly classified git invocation.
#[derive(Clone, Copy)]
pub(super) enum GitFailure {
    /// Invalid user-supplied revision or path.
    NotFound,
    /// A resolved object failed during repository plumbing.
    Internal,
}

fn output_bytes(
    output: std::process::Output,
    command: &str,
    failure: GitFailure,
) -> Result<Vec<u8>, ApiError> {
    if output.status.success() {
        return Ok(output.stdout);
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    match failure {
        GitFailure::NotFound => Err(ApiError::NotFound(stderr)),
        GitFailure::Internal => {
            let status = output.status.code();
            tracing::error!(command, ?status, stderr, "browsing git plumbing failed");
            Err(ApiError::Internal(format!(
                "git `{command}` exited {status:?}: {stderr}"
            )))
        }
    }
}

pub(super) async fn git(
    local: &gitcask_git::LocalRepo,
    args: Vec<String>,
    failure: GitFailure,
) -> Result<Vec<u8>, ApiError> {
    let command = args.join(" ");
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = local.git(&refs).await.map_err(|error| {
        tracing::error!(command, %error, "browsing git subprocess failed");
        ApiError::Internal(format!("git `{command}`: {error}"))
    })?;
    output_bytes(output, &command, failure)
}

pub(super) fn parse_commit_record(record: &str) -> Option<Commit> {
    let mut parts = record.split('\0');
    let sha = parts.next()?.trim().to_string();
    if sha.is_empty() {
        return None;
    }
    let parents = parts
        .next()?
        .split_whitespace()
        .map(str::to_string)
        .collect();
    Some(
        Commit {
            sha,
            parents,
            author: parts.next()?.to_string(),
            author_email: parts.next()?.to_string(),
            author_date: parts.next()?.to_string(),
            committer: parts.next()?.to_string(),
            commit_date: parts.next()?.to_string(),
            subject: parts.next()?.to_string(),
            body: String::new(),
            trailers: Vec::new(),
        }
        .with_body(parts.next().unwrap_or("")),
    )
}

pub(super) fn parse_commits(bytes: &[u8]) -> Vec<Commit> {
    String::from_utf8_lossy(bytes)
        .split('\x1e')
        .filter_map(parse_commit_record)
        .collect()
}

pub(super) fn log_format() -> &'static str {
    "%x1e%H%x00%P%x00%an%x00%ae%x00%aI%x00%cn%x00%cI%x00%s%x00%b%x00"
}

pub(super) fn parse_stats(bytes: &[u8]) -> Vec<Stat> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            if fields.len() < 3
                || (!fields[0]
                    .chars()
                    .all(|character| character.is_ascii_digit())
                    && fields[0] != "-")
            {
                return None;
            }
            Some(Stat {
                path: normalize_rename(fields[2]),
                additions: if fields[0] == "-" {
                    -1
                } else {
                    fields[0].parse().unwrap_or(-1)
                },
                deletions: if fields[1] == "-" {
                    -1
                } else {
                    fields[1].parse().unwrap_or(-1)
                },
            })
        })
        .collect()
}

/// `git --numstat -M` prints renames as `old => new` or
/// `prefix/{old => new}/suffix`; return the new path.
fn normalize_rename(path: &str) -> String {
    if let (Some(open), Some(close)) = (path.find('{'), path.rfind('}')) {
        if open < close {
            let inner = &path[open + 1..close];
            if let Some((_, new)) = inner.split_once(" => ") {
                let mut output = String::with_capacity(path.len());
                output.push_str(&path[..open]);
                output.push_str(new);
                output.push_str(&path[close + 1..]);
                return output.replace("//", "/");
            }
        }
    }
    if let Some((_, new)) = path.split_once(" => ") {
        return new.to_string();
    }
    path.to_string()
}

pub(super) fn parse_compare_counts(bytes: &[u8]) -> Result<(usize, usize), ApiError> {
    let text = String::from_utf8_lossy(bytes);
    let mut fields = text.split_whitespace();
    let behind_by = fields
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| ApiError::Internal("invalid compare behind count".into()))?;
    let ahead_by = fields
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| ApiError::Internal("invalid compare ahead count".into()))?;
    Ok((ahead_by, behind_by))
}

pub(super) fn parse_compare_files(statuses: &[u8], stats: &[u8]) -> Vec<CompareFile> {
    let stats_by_path: HashMap<String, (i64, i64)> = parse_stats(stats)
        .into_iter()
        .map(|stat| (stat.path, (stat.additions, stat.deletions)))
        .collect();
    String::from_utf8_lossy(statuses)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let code = fields.next()?;
            let first_path = fields.next()?;
            let path = if code.starts_with('R') {
                fields.next().unwrap_or(first_path)
            } else {
                first_path
            };
            let status = match code.as_bytes().first()? {
                b'A' => "added",
                b'D' => "deleted",
                b'R' => "renamed",
                _ => "modified",
            };
            let (additions, deletions) = stats_by_path.get(path).copied().unwrap_or((0, 0));
            Some(CompareFile {
                path: path.to_string(),
                status,
                additions,
                deletions,
            })
        })
        .collect()
}

pub(super) fn bounded_patch(bytes: &[u8]) -> (String, bool) {
    let truncated = bytes.len() > MAX_PATCH;
    let end = bytes.len().min(MAX_PATCH);
    (
        String::from_utf8_lossy(&bytes[..end]).into_owned(),
        truncated,
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn rename_paths() {
        assert_eq!(
            super::normalize_rename("src/{main.rs => app.rs}"),
            "src/app.rs"
        );
        assert_eq!(super::normalize_rename("{a => b}/x.rs"), "b/x.rs");
        assert_eq!(super::normalize_rename("a/{ => sub}/x.rs"), "a/sub/x.rs");
        assert_eq!(super::normalize_rename("old.rs => new.rs"), "new.rs");
        assert_eq!(super::normalize_rename("plain.rs"), "plain.rs");
    }
}
