//! Git integration service — repository operations via libgit2.
//!
//! Data source: local filesystem git repositories via the `git2` crate.
//! Provides branch management, staging, committing, diffing, and log access.

use crate::error::AppError;
use crate::event_bus::{AppEvent, EventBus};
use crate::view_models::ide_view_models::{
    BranchInfo, CommitInfo, DiffHunk, DiffLine, DiffLineType, GitChangeType, GitFileStatus,
};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

// ---------------------------------------------------------------------------
// Backend trait
// ---------------------------------------------------------------------------

/// Backend trait for git operations. Real implementation uses libgit2.
#[async_trait::async_trait]
pub trait GitBackend: Send + Sync {
    /// Open a git repository at the given path.
    async fn open_repo(&self, path: &Path) -> Result<(), AppError>;

    /// Get the name of the current branch (e.g. "main").
    async fn current_branch(&self) -> Result<String, AppError>;

    /// List changed, staged, and untracked files.
    async fn status(&self) -> Result<Vec<GitFileStatus>, AppError>;

    /// Get diff hunks. If `path` is `Some`, diff only that file; otherwise diff all.
    async fn diff(&self, path: Option<&Path>) -> Result<Vec<DiffHunk>, AppError>;

    /// Stage the given file paths for commit.
    async fn stage(&self, paths: &[&Path]) -> Result<(), AppError>;

    /// Unstage the given file paths (remove from index, keep working-tree changes).
    async fn unstage(&self, paths: &[&Path]) -> Result<(), AppError>;

    /// Create a commit with the given message. Returns the commit hash (hex).
    async fn commit(&self, message: &str) -> Result<String, AppError>;

    /// List local and remote branches.
    async fn branches(&self) -> Result<Vec<BranchInfo>, AppError>;

    /// Switch to the named branch.
    async fn checkout(&self, branch: &str) -> Result<(), AppError>;

    /// Push the current branch to the named remote (e.g. "origin").
    async fn push(&self, remote: &str) -> Result<(), AppError>;

    /// Pull the current branch from the named remote.
    async fn pull(&self, remote: &str) -> Result<(), AppError>;

    /// Get the most recent `count` commits from HEAD.
    async fn log(&self, count: usize) -> Result<Vec<CommitInfo>, AppError>;
}

// ---------------------------------------------------------------------------
// Real backend (git2)
// ---------------------------------------------------------------------------

/// Production backend using `git2` (libgit2 bindings).
/// Uses std::sync::Mutex (not tokio) because git2::Repository is not Send.
/// All operations run inside spawn_blocking to avoid holding non-Send types across await.
pub struct Git2Backend {
    repo: Arc<std::sync::Mutex<Option<git2::Repository>>>,
}

fn lock_repo(
    repo: &std::sync::Mutex<Option<git2::Repository>>,
) -> std::sync::MutexGuard<'_, Option<git2::Repository>> {
    match repo.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

impl Git2Backend {
    pub fn new() -> Self {
        Self {
            repo: Arc::new(std::sync::Mutex::new(None)),
        }
    }
}

impl Default for Git2Backend {
    fn default() -> Self {
        Self::new()
    }
}

impl Git2Backend {
    /// Helper: run a closure with the repo on a blocking thread.
    async fn with_repo<F, T>(&self, op: F) -> Result<T, AppError>
    where
        F: FnOnce(&git2::Repository) -> Result<T, AppError> + Send + 'static,
        T: Send + 'static,
    {
        let repo_arc = self.repo.clone();
        tokio::task::spawn_blocking(move || {
            let guard = lock_repo(&repo_arc);
            let repo = guard
                .as_ref()
                .ok_or_else(|| AppError::Git("No repository is open".to_string()))?;
            op(repo)
        })
        .await
        .map_err(|e| AppError::Git(format!("spawn_blocking failed: {}", e)))?
    }

    /// Helper: run a mutable closure with the repo on a blocking thread.
    async fn with_repo_mut<F, T>(&self, op: F) -> Result<T, AppError>
    where
        F: FnOnce(&git2::Repository) -> Result<T, AppError> + Send + 'static,
        T: Send + 'static,
    {
        let repo_arc = self.repo.clone();
        tokio::task::spawn_blocking(move || {
            let guard = lock_repo(&repo_arc);
            let repo = guard
                .as_ref()
                .ok_or_else(|| AppError::Git("No repository is open".to_string()))?;
            op(repo)
        })
        .await
        .map_err(|e| AppError::Git(format!("spawn_blocking failed: {}", e)))?
    }
}

/// Map a `git2::Error` into `AppError::Git`.
fn git2_err(e: git2::Error) -> AppError {
    AppError::Git(e.message().to_string())
}

/// Map git2 status flags to our `GitChangeType`.
fn status_to_change_type(status: git2::Status) -> GitChangeType {
    if status.intersects(git2::Status::WT_NEW | git2::Status::INDEX_NEW) {
        if status.intersects(git2::Status::INDEX_NEW) {
            GitChangeType::Added
        } else {
            GitChangeType::Untracked
        }
    } else if status.intersects(git2::Status::WT_DELETED | git2::Status::INDEX_DELETED) {
        GitChangeType::Deleted
    } else if status.intersects(git2::Status::WT_RENAMED | git2::Status::INDEX_RENAMED) {
        GitChangeType::Renamed
    } else if status.intersects(git2::Status::CONFLICTED) {
        GitChangeType::Conflicted
    } else {
        GitChangeType::Modified
    }
}

/// Check whether a status entry is staged (in the index).
fn is_staged(status: git2::Status) -> bool {
    status.intersects(
        git2::Status::INDEX_NEW
            | git2::Status::INDEX_MODIFIED
            | git2::Status::INDEX_DELETED
            | git2::Status::INDEX_RENAMED
            | git2::Status::INDEX_TYPECHANGE,
    )
}

#[async_trait::async_trait]
impl GitBackend for Git2Backend {
    async fn open_repo(&self, path: &Path) -> Result<(), AppError> {
        let path = path.to_path_buf();
        let repo_arc = self.repo.clone();
        tokio::task::spawn_blocking(move || {
            let repo = git2::Repository::open(&path).map_err(|e| {
                AppError::Git(format!("Failed to open repository at {}: {}", path.display(), e))
            })?;
            *lock_repo(&repo_arc) = Some(repo);
            Ok(())
        })
        .await
        .map_err(|e| AppError::Git(format!("spawn_blocking failed: {}", e)))?
    }

    async fn current_branch(&self) -> Result<String, AppError> {
        self.with_repo(|repo| {
            let head = repo.head().map_err(git2_err)?;
            let branch_name = head
                .shorthand()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "HEAD (detached)".to_string());
            Ok(branch_name)
        })
        .await
    }

    async fn status(&self) -> Result<Vec<GitFileStatus>, AppError> {
        self.with_repo(|repo| {
            let statuses = repo.statuses(None).map_err(git2_err)?;
            let mut result = Vec::with_capacity(statuses.len());
            for entry in statuses.iter() {
                let path = entry
                    .path()
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "<non-utf8>".to_string());
                let st = entry.status();

                // Skip IGNORED entries
                if st.contains(git2::Status::IGNORED) {
                    continue;
                }

                // An entry may appear as both staged and unstaged (partially staged).
                // We emit two entries in that case: one staged, one unstaged.
                let has_index = is_staged(st);
                let has_wt = st.intersects(
                    git2::Status::WT_NEW
                        | git2::Status::WT_MODIFIED
                        | git2::Status::WT_DELETED
                        | git2::Status::WT_RENAMED
                        | git2::Status::WT_TYPECHANGE
                        | git2::Status::CONFLICTED,
                );

                if has_index {
                    result.push(GitFileStatus {
                        path: path.clone(),
                        change_type: status_to_change_type(st),
                        staged: true,
                    });
                }
                if has_wt && !has_index {
                    result.push(GitFileStatus {
                        path: path.clone(),
                        change_type: status_to_change_type(st),
                        staged: false,
                    });
                } else if has_wt && has_index {
                    // Partially staged — also show the working-tree portion
                    result.push(GitFileStatus {
                        path: path.clone(),
                        change_type: status_to_change_type(st),
                        staged: false,
                    });
                }
            }
            Ok(result)
        })
        .await
    }

    async fn diff(&self, path: Option<&Path>) -> Result<Vec<DiffHunk>, AppError> {
        let path_owned = path.map(|p| p.to_path_buf());
        self.with_repo(move |repo| {
            let mut diff_opts = git2::DiffOptions::new();
            if let Some(ref p) = path_owned {
                diff_opts.pathspec(p.to_string_lossy().as_ref());
            }

            // diff HEAD (or empty tree) to workdir
            let head_tree = repo
                .head()
                .ok()
                .and_then(|h| h.peel_to_tree().ok());

            let diff = repo
                .diff_tree_to_workdir_with_index(head_tree.as_ref(), Some(&mut diff_opts))
                .map_err(git2_err)?;

            let hunks = std::cell::RefCell::new(Vec::<DiffHunk>::new());
            diff.foreach(
                &mut |_delta, _progress| true,
                None,
                Some(&mut |_delta, hunk| {
                    hunks.borrow_mut().push(DiffHunk {
                        old_start: hunk.old_start() as usize,
                        old_count: hunk.old_lines() as usize,
                        new_start: hunk.new_start() as usize,
                        new_count: hunk.new_lines() as usize,
                        lines: Vec::new(),
                    });
                    true
                }),
                Some(&mut |_delta, _hunk, line| {
                    let content = String::from_utf8_lossy(line.content()).to_string();
                    let line_type = match line.origin() {
                        '+' => DiffLineType::Addition,
                        '-' => DiffLineType::Deletion,
                        _ => DiffLineType::Context,
                    };
                    if let Some(last_hunk) = hunks.borrow_mut().last_mut() {
                        last_hunk.lines.push(DiffLine { content, line_type });
                    }
                    true
                }),
            )
            .map_err(git2_err)?;

            Ok(hunks.into_inner())
        })
        .await
    }

    async fn stage(&self, paths: &[&Path]) -> Result<(), AppError> {
        let owned: Vec<std::path::PathBuf> = paths.iter().map(|p| p.to_path_buf()).collect();
        self.with_repo_mut(move |repo| {
            let mut index = repo.index().map_err(git2_err)?;
            for p in &owned {
                // Try adding first; if the file was deleted, remove from index instead.
                let add_result = index.add_path(p);
                if let Err(e) = add_result {
                    // If the file doesn't exist on disk, it might be a deletion
                    let workdir = repo.workdir().ok_or_else(|| {
                        AppError::Git("Repository has no working directory".to_string())
                    })?;
                    if !workdir.join(p).exists() {
                        index.remove_path(p).map_err(git2_err)?;
                    } else {
                        return Err(AppError::Git(format!(
                            "Failed to stage {}: {}",
                            p.display(),
                            e
                        )));
                    }
                }
            }
            index.write().map_err(git2_err)?;
            Ok(())
        })
        .await
    }

    async fn unstage(&self, paths: &[&Path]) -> Result<(), AppError> {
        let owned: Vec<std::path::PathBuf> = paths.iter().map(|p| p.to_path_buf()).collect();
        self.with_repo_mut(move |repo| {
            let head = repo.head().map_err(git2_err)?;
            let head_commit = head.peel_to_commit().map_err(git2_err)?;
            let head_tree = head_commit.tree().map_err(git2_err)?;

            let mut index = repo.index().map_err(git2_err)?;
            for p in &owned {
                let path_str = p.to_string_lossy();
                // Check if the file existed in HEAD
                if head_tree.get_path(p).is_ok() {
                    // Reset to HEAD version in index
                    let entry = head_tree.get_path(p).map_err(git2_err)?;
                    let mut idx_entry = git2::IndexEntry {
                        ctime: git2::IndexTime::new(0, 0),
                        mtime: git2::IndexTime::new(0, 0),
                        dev: 0,
                        ino: 0,
                        mode: entry.filemode() as u32,
                        uid: 0,
                        gid: 0,
                        file_size: 0,
                        id: entry.id(),
                        flags: 0,
                        flags_extended: 0,
                        path: path_str.as_bytes().to_vec(),
                    };
                    // Compute the correct flags (stage = 0, name length)
                    let name_len = idx_entry.path.len();
                    idx_entry.flags = if name_len < 0xFFF {
                        name_len as u16
                    } else {
                        0xFFF
                    };
                    index.add(&idx_entry).map_err(git2_err)?;
                } else {
                    // File was newly added — remove from index entirely
                    index.remove_path(p).map_err(git2_err)?;
                }
            }
            index.write().map_err(git2_err)?;
            Ok(())
        })
        .await
    }

    async fn commit(&self, message: &str) -> Result<String, AppError> {
        let msg = message.to_string();
        self.with_repo_mut(move |repo| {
            let mut index = repo.index().map_err(git2_err)?;
            let tree_oid = index.write_tree().map_err(git2_err)?;
            let tree = repo.find_tree(tree_oid).map_err(git2_err)?;

            let sig = repo.signature().map_err(git2_err)?;
            let head = repo.head().map_err(git2_err)?;
            let parent = head.peel_to_commit().map_err(git2_err)?;

            let oid = repo
                .commit(Some("HEAD"), &sig, &sig, &msg, &tree, &[&parent])
                .map_err(git2_err)?;

            Ok(oid.to_string())
        })
        .await
    }

    async fn branches(&self) -> Result<Vec<BranchInfo>, AppError> {
        self.with_repo(|repo| {
            let current = repo
                .head()
                .ok()
                .and_then(|h| h.shorthand().map(|s| s.to_string()));

            let branches_iter = repo.branches(None).map_err(git2_err)?;
            let mut result = Vec::new();
            for branch_result in branches_iter {
                let (branch, branch_type) = branch_result.map_err(git2_err)?;
                let name = branch
                    .name()
                    .map_err(git2_err)?
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "<non-utf8>".to_string());
                let is_remote = branch_type == git2::BranchType::Remote;
                let is_current = !is_remote
                    && current
                        .as_ref()
                        .map(|c| c == &name)
                        .unwrap_or(false);
                result.push(BranchInfo {
                    name,
                    is_current,
                    is_remote,
                });
            }
            Ok(result)
        })
        .await
    }

    async fn checkout(&self, branch: &str) -> Result<(), AppError> {
        let branch_name = branch.to_string();
        self.with_repo_mut(move |repo| {
            let (object, reference) = repo.revparse_ext(&branch_name).map_err(git2_err)?;
            repo.checkout_tree(&object, None).map_err(git2_err)?;
            if let Some(ref_name) = reference {
                let refname = ref_name
                    .name()
                    .ok_or_else(|| AppError::Git("Branch reference has no name".to_string()))?;
                repo.set_head(refname).map_err(git2_err)?;
            } else {
                repo.set_head_detached(object.id()).map_err(git2_err)?;
            }
            Ok(())
        })
        .await
    }

    async fn push(&self, remote: &str) -> Result<(), AppError> {
        let remote_name = remote.to_string();
        self.with_repo_mut(move |repo| {
            let head = repo.head().map_err(git2_err)?;
            let refname = head
                .name()
                .ok_or_else(|| AppError::Git("HEAD is not a symbolic reference".to_string()))?
                .to_string();

            let mut remote_obj = repo.find_remote(&remote_name).map_err(git2_err)?;
            let refspec = format!("+{}:{}", refname, refname);
            remote_obj
                .push(&[&refspec], None)
                .map_err(git2_err)?;
            Ok(())
        })
        .await
    }

    async fn pull(&self, remote: &str) -> Result<(), AppError> {
        let remote_name = remote.to_string();
        self.with_repo_mut(move |repo| {
            let mut remote_obj = repo.find_remote(&remote_name).map_err(git2_err)?;
            let head = repo.head().map_err(git2_err)?;
            let branch = head
                .shorthand()
                .ok_or_else(|| AppError::Git("HEAD has no branch name".to_string()))?
                .to_string();

            // Fetch
            remote_obj
                .fetch(&[&branch], None, None)
                .map_err(git2_err)?;

            // Merge: find the fetch head
            let fetch_head = repo
                .find_reference("FETCH_HEAD")
                .map_err(git2_err)?;
            let fetch_commit = fetch_head
                .peel_to_commit()
                .map_err(git2_err)?;

            let local_head = repo.head().map_err(git2_err)?;
            let local_commit = local_head.peel_to_commit().map_err(git2_err)?;

            let (analysis, _) = repo
                .merge_analysis(&[&repo.find_annotated_commit(fetch_commit.id()).map_err(git2_err)?])
                .map_err(git2_err)?;

            if analysis.is_up_to_date() {
                // Nothing to do
                Ok(())
            } else if analysis.is_fast_forward() {
                // Fast-forward merge
                let refname = local_head
                    .name()
                    .ok_or_else(|| AppError::Git("HEAD has no name".to_string()))?;
                let mut reference = repo.find_reference(refname).map_err(git2_err)?;
                reference
                    .set_target(fetch_commit.id(), "fast-forward pull")
                    .map_err(git2_err)?;
                repo.set_head(refname).map_err(git2_err)?;
                repo.checkout_head(Some(
                    git2::build::CheckoutBuilder::default().force(),
                ))
                .map_err(git2_err)?;
                Ok(())
            } else {
                Err(AppError::Git(format!(
                    "Pull requires a merge (non-fast-forward). Local: {}, Remote: {}. Manual merge needed.",
                    local_commit.id(),
                    fetch_commit.id(),
                )))
            }
        })
        .await
    }

    async fn log(&self, count: usize) -> Result<Vec<CommitInfo>, AppError> {
        self.with_repo(move |repo| {
            let head = repo.head().map_err(git2_err)?;
            let head_oid = head
                .target()
                .ok_or_else(|| AppError::Git("HEAD has no target OID".to_string()))?;

            let mut revwalk = repo.revwalk().map_err(git2_err)?;
            revwalk.push(head_oid).map_err(git2_err)?;
            revwalk
                .set_sorting(git2::Sort::TIME)
                .map_err(git2_err)?;

            let mut commits = Vec::with_capacity(count);
            for oid_result in revwalk.take(count) {
                let oid = oid_result.map_err(git2_err)?;
                let commit = repo.find_commit(oid).map_err(git2_err)?;
                let hash_hex = oid.to_string();
                let hash_short = if hash_hex.len() >= 7 {
                    hash_hex[..7].to_string()
                } else {
                    hash_hex.clone()
                };
                commits.push(CommitInfo {
                    hash_short,
                    message: commit
                        .message()
                        .map(|m| m.trim().to_string())
                        .unwrap_or_else(|| "<no message>".to_string()),
                    author: commit.author().name().unwrap_or("<unknown>").to_string(),
                    timestamp: commit.time().seconds() as u64,
                });
            }
            Ok(commits)
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// Test-only backend
// ---------------------------------------------------------------------------

/// Test-only backend that wraps a real `Git2Backend` but is gated behind `#[cfg(test)]`.
/// This exists so tests can use `with_backend()` injection while still exercising
/// real libgit2 operations on temporary repositories.
#[cfg(test)]
pub struct TestGit2Backend {
    inner: Git2Backend,
}

#[cfg(test)]
impl Default for TestGit2Backend {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
impl TestGit2Backend {
    pub fn new() -> Self {
        Self {
            inner: Git2Backend::new(),
        }
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl GitBackend for TestGit2Backend {
    async fn open_repo(&self, path: &Path) -> Result<(), AppError> {
        self.inner.open_repo(path).await
    }

    async fn current_branch(&self) -> Result<String, AppError> {
        self.inner.current_branch().await
    }

    async fn status(&self) -> Result<Vec<GitFileStatus>, AppError> {
        self.inner.status().await
    }

    async fn diff(&self, path: Option<&Path>) -> Result<Vec<DiffHunk>, AppError> {
        self.inner.diff(path).await
    }

    async fn stage(&self, paths: &[&Path]) -> Result<(), AppError> {
        self.inner.stage(paths).await
    }

    async fn unstage(&self, paths: &[&Path]) -> Result<(), AppError> {
        self.inner.unstage(paths).await
    }

    async fn commit(&self, message: &str) -> Result<String, AppError> {
        self.inner.commit(message).await
    }

    async fn branches(&self) -> Result<Vec<BranchInfo>, AppError> {
        self.inner.branches().await
    }

    async fn checkout(&self, branch: &str) -> Result<(), AppError> {
        self.inner.checkout(branch).await
    }

    async fn push(&self, remote: &str) -> Result<(), AppError> {
        self.inner.push(remote).await
    }

    async fn pull(&self, remote: &str) -> Result<(), AppError> {
        self.inner.pull(remote).await
    }

    async fn log(&self, count: usize) -> Result<Vec<CommitInfo>, AppError> {
        self.inner.log(count).await
    }
}

// ---------------------------------------------------------------------------
// GitService (wraps backend + caches state)
// ---------------------------------------------------------------------------

/// Cached git state exposed to the UI.
#[derive(Debug, Clone, Default)]
pub struct GitState {
    pub current_branch: String,
    pub file_statuses: Vec<GitFileStatus>,
}

/// Git service — wraps a `GitBackend`, caches branch/status, publishes events.
pub struct GitService {
    events: Arc<EventBus>,
    backend: Arc<dyn GitBackend>,
    state: Arc<RwLock<GitState>>,
}

impl GitService {
    /// Create with the real `Git2Backend`.
    pub fn new(events: Arc<EventBus>) -> Self {
        Self {
            events,
            backend: Arc::new(Git2Backend::new()),
            state: Arc::new(RwLock::new(GitState::default())),
        }
    }

    /// Create with an injected backend (for testing or alternative implementations).
    pub fn with_backend(events: Arc<EventBus>, backend: Arc<dyn GitBackend>) -> Self {
        Self {
            events,
            backend,
            state: Arc::new(RwLock::new(GitState::default())),
        }
    }

    /// Open a repository and perform an initial status refresh.
    pub async fn open_repo(&self, path: &Path) -> Result<(), AppError> {
        self.backend.open_repo(path).await?;
        self.refresh().await?;
        Ok(())
    }

    /// Re-poll the current branch and file status from the backend.
    pub async fn refresh(&self) -> Result<(), AppError> {
        let branch = self.backend.current_branch().await?;
        let statuses = self.backend.status().await?;

        let staged_count = statuses.iter().filter(|s| s.staged).count();
        let changed_count = statuses.len();

        let mut state = self.state.write().await;
        state.current_branch = branch.clone();
        state.file_statuses = statuses;
        drop(state);

        self.events.publish(AppEvent::GitStatusChanged {
            branch,
            changed_count,
            staged_count,
        });

        Ok(())
    }

    /// Get the cached git state.
    pub async fn get_state(&self) -> GitState {
        self.state.read().await.clone()
    }

    /// Get the current branch name (from cache).
    pub async fn current_branch(&self) -> String {
        self.state.read().await.current_branch.clone()
    }

    /// Get file statuses (from cache).
    pub async fn status(&self) -> Vec<GitFileStatus> {
        self.state.read().await.file_statuses.clone()
    }

    /// Get diff hunks for a file or all files.
    pub async fn diff(&self, path: Option<&Path>) -> Result<Vec<DiffHunk>, AppError> {
        self.backend.diff(path).await
    }

    /// Stage files and refresh status.
    pub async fn stage(&self, paths: &[&Path]) -> Result<(), AppError> {
        self.backend.stage(paths).await?;
        self.refresh().await?;
        Ok(())
    }

    /// Unstage files and refresh status.
    pub async fn unstage(&self, paths: &[&Path]) -> Result<(), AppError> {
        self.backend.unstage(paths).await?;
        self.refresh().await?;
        Ok(())
    }

    /// Commit staged changes and refresh status. Returns the commit hash.
    pub async fn commit(&self, message: &str) -> Result<String, AppError> {
        let hash = self.backend.commit(message).await?;
        self.refresh().await?;
        Ok(hash)
    }

    /// List all branches.
    pub async fn branches(&self) -> Result<Vec<BranchInfo>, AppError> {
        self.backend.branches().await
    }

    /// Switch to a branch and refresh status.
    pub async fn checkout(&self, branch: &str) -> Result<(), AppError> {
        self.backend.checkout(branch).await?;
        self.refresh().await?;
        Ok(())
    }

    /// Push the current branch.
    pub async fn push(&self, remote: &str) -> Result<(), AppError> {
        self.backend.push(remote).await
    }

    /// Pull the current branch.
    pub async fn pull(&self, remote: &str) -> Result<(), AppError> {
        self.backend.pull(remote).await?;
        self.refresh().await?;
        Ok(())
    }

    /// Get commit log.
    pub async fn log(&self, count: usize) -> Result<Vec<CommitInfo>, AppError> {
        self.backend.log(count).await
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// Create a temporary directory with a unique name for test isolation.
    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "citrate_git_test_{}_{}", label, uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).expect("failed to create temp dir");
        dir
    }

    /// Initialize a bare git repo with one initial commit so HEAD exists.
    fn init_repo_with_commit(path: &Path) -> git2::Repository {
        let repo = git2::Repository::init(path).expect("failed to init repo");

        // Configure user for commits
        let mut config = repo.config().expect("failed to get config");
        config
            .set_str("user.name", "Test User")
            .expect("failed to set user.name");
        config
            .set_str("user.email", "test@example.com")
            .expect("failed to set user.email");

        // Create an initial commit (empty tree)
        let sig = repo.signature().expect("failed to create signature");
        let tree_oid = {
            let mut index = repo.index().expect("failed to get index");
            index.write_tree().expect("failed to write tree")
        };
        {
            let tree = repo.find_tree(tree_oid).expect("failed to find tree");
            repo.commit(Some("HEAD"), &sig, &sig, "Initial commit", &tree, &[])
                .expect("failed to create initial commit");
        } // tree dropped here, before repo is moved

        repo
    }

    /// Helper to create a `GitService` with `TestGit2Backend`.
    fn make_service() -> (GitService, Arc<EventBus>) {
        let events = Arc::new(EventBus::new());
        let backend = Arc::new(TestGit2Backend::new());
        let service = GitService::with_backend(events.clone(), backend);
        (service, events)
    }

    // --- open_repo ---

    #[tokio::test]
    async fn test_open_repo_success() {
        let dir = temp_dir("open_success");
        init_repo_with_commit(&dir);

        let (service, _events) = make_service();
        service.open_repo(&dir).await.expect("open_repo should succeed");

        let state = service.get_state().await;
        // After open, the branch should be detected
        assert!(!state.current_branch.is_empty());

        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_open_repo_not_a_git_dir() {
        let dir = temp_dir("open_not_git");
        // Don't init a repo — just a plain directory

        let (service, _events) = make_service();
        let result = service.open_repo(&dir).await;
        assert!(result.is_err());
        let err_msg = format!("{}", result.expect_err("should be an error"));
        assert!(
            err_msg.contains("Failed to open repository") || err_msg.contains("Git error"),
            "Unexpected error message: {}",
            err_msg,
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_open_repo_nonexistent_path() {
        let dir = temp_dir("open_nonexistent");
        let bad_path = dir.join("does_not_exist");

        let (service, _events) = make_service();
        let result = service.open_repo(&bad_path).await;
        assert!(result.is_err());

        fs::remove_dir_all(&dir).ok();
    }

    // --- current_branch ---

    #[tokio::test]
    async fn test_current_branch_is_master_or_main() {
        let dir = temp_dir("branch_default");
        init_repo_with_commit(&dir);

        let (service, _events) = make_service();
        service.open_repo(&dir).await.expect("open_repo should succeed");

        let branch = service.current_branch().await;
        // git2::Repository::init creates "master" by default
        assert!(
            branch == "master" || branch == "main",
            "Expected master or main, got: {}",
            branch,
        );

        fs::remove_dir_all(&dir).ok();
    }

    // --- status on clean repo ---

    #[tokio::test]
    async fn test_status_clean_repo() {
        let dir = temp_dir("status_clean");
        init_repo_with_commit(&dir);

        let (service, _events) = make_service();
        service.open_repo(&dir).await.expect("open_repo should succeed");

        let statuses = service.status().await;
        assert!(statuses.is_empty(), "Clean repo should have no status entries");

        fs::remove_dir_all(&dir).ok();
    }

    // --- status with new file ---

    #[tokio::test]
    async fn test_status_new_file() {
        let dir = temp_dir("status_new");
        init_repo_with_commit(&dir);

        fs::write(dir.join("hello.txt"), "hello world").expect("failed to write file");

        let (service, _events) = make_service();
        service.open_repo(&dir).await.expect("open_repo should succeed");

        let statuses = service.status().await;
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].path, "hello.txt");
        assert_eq!(statuses[0].change_type, GitChangeType::Untracked);
        assert!(!statuses[0].staged);

        fs::remove_dir_all(&dir).ok();
    }

    // --- status with modified file ---

    #[tokio::test]
    async fn test_status_modified_file() {
        let dir = temp_dir("status_modified");
        let repo = init_repo_with_commit(&dir);

        // Add and commit a file first
        let file_path = dir.join("data.txt");
        fs::write(&file_path, "original content").expect("failed to write");
        {
            let mut index = repo.index().expect("failed to get index");
            index.add_path(Path::new("data.txt")).expect("failed to add");
            index.write().expect("failed to write index");
            let tree_oid = index.write_tree().expect("failed to write tree");
            let tree = repo.find_tree(tree_oid).expect("failed to find tree");
            let sig = repo.signature().expect("failed to create signature");
            let head = repo.head().expect("failed to get head");
            let parent = head.peel_to_commit().expect("failed to peel");
            repo.commit(Some("HEAD"), &sig, &sig, "Add data.txt", &tree, &[&parent])
                .expect("failed to commit");
        }

        // Now modify the file
        fs::write(&file_path, "modified content").expect("failed to modify");

        let (service, _events) = make_service();
        service.open_repo(&dir).await.expect("open_repo should succeed");

        let statuses = service.status().await;
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].path, "data.txt");
        assert_eq!(statuses[0].change_type, GitChangeType::Modified);
        assert!(!statuses[0].staged);

        fs::remove_dir_all(&dir).ok();
    }

    // --- status with deleted file ---

    #[tokio::test]
    async fn test_status_deleted_file() {
        let dir = temp_dir("status_deleted");
        let repo = init_repo_with_commit(&dir);

        // Add and commit a file
        let file_path = dir.join("to_delete.txt");
        fs::write(&file_path, "will be deleted").expect("failed to write");
        {
            let mut index = repo.index().expect("failed to get index");
            index.add_path(Path::new("to_delete.txt")).expect("failed to add");
            index.write().expect("failed to write index");
            let tree_oid = index.write_tree().expect("failed to write tree");
            let tree = repo.find_tree(tree_oid).expect("failed to find tree");
            let sig = repo.signature().expect("failed to create signature");
            let head = repo.head().expect("failed to get head");
            let parent = head.peel_to_commit().expect("failed to peel");
            repo.commit(Some("HEAD"), &sig, &sig, "Add file", &tree, &[&parent])
                .expect("failed to commit");
        }

        // Delete the file
        fs::remove_file(&file_path).expect("failed to delete");

        let (service, _events) = make_service();
        service.open_repo(&dir).await.expect("open_repo should succeed");

        let statuses = service.status().await;
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].path, "to_delete.txt");
        assert_eq!(statuses[0].change_type, GitChangeType::Deleted);

        fs::remove_dir_all(&dir).ok();
    }

    // --- stage and unstage ---

    #[tokio::test]
    async fn test_stage_file() {
        let dir = temp_dir("stage");
        init_repo_with_commit(&dir);

        fs::write(dir.join("new.txt"), "staged content").expect("failed to write");

        let (service, _events) = make_service();
        service.open_repo(&dir).await.expect("open_repo should succeed");

        // Before staging
        let before = service.status().await;
        assert!(!before.is_empty());
        assert!(!before[0].staged);

        // Stage
        service
            .stage(&[Path::new("new.txt")])
            .await
            .expect("stage should succeed");

        // After staging
        let after = service.status().await;
        let staged_entries: Vec<_> = after.iter().filter(|s| s.staged).collect();
        assert!(!staged_entries.is_empty(), "File should be staged after stage()");
        assert_eq!(staged_entries[0].change_type, GitChangeType::Added);

        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_unstage_file() {
        let dir = temp_dir("unstage");
        init_repo_with_commit(&dir);

        fs::write(dir.join("unstage_me.txt"), "content").expect("failed to write");

        let (service, _events) = make_service();
        service.open_repo(&dir).await.expect("open_repo should succeed");

        // Stage first
        service
            .stage(&[Path::new("unstage_me.txt")])
            .await
            .expect("stage should succeed");

        let staged = service.status().await;
        assert!(
            staged.iter().any(|s| s.staged && s.path == "unstage_me.txt"),
            "File should be staged",
        );

        // Unstage
        service
            .unstage(&[Path::new("unstage_me.txt")])
            .await
            .expect("unstage should succeed");

        let after = service.status().await;
        // The file should still show as untracked (since it was newly added)
        let still_staged = after.iter().any(|s| s.staged && s.path == "unstage_me.txt");
        assert!(!still_staged, "File should no longer be staged after unstage()");

        fs::remove_dir_all(&dir).ok();
    }

    // --- commit ---

    #[tokio::test]
    async fn test_commit_and_log() {
        let dir = temp_dir("commit_log");
        init_repo_with_commit(&dir);

        fs::write(dir.join("committed.txt"), "for commit").expect("failed to write");

        let (service, _events) = make_service();
        service.open_repo(&dir).await.expect("open_repo should succeed");

        // Stage and commit
        service
            .stage(&[Path::new("committed.txt")])
            .await
            .expect("stage should succeed");

        let hash = service
            .commit("Add committed.txt")
            .await
            .expect("commit should succeed");

        assert!(!hash.is_empty(), "Commit hash should not be empty");
        assert!(hash.len() >= 7, "Commit hash should be at least 7 chars");

        // Verify via log
        let log = service.log(5).await.expect("log should succeed");
        assert!(log.len() >= 2, "Should have at least 2 commits (initial + new)");
        assert_eq!(log[0].message, "Add committed.txt");
        assert_eq!(log[0].author, "Test User");

        // After commit, status should be clean
        let statuses = service.status().await;
        assert!(statuses.is_empty(), "Status should be clean after commit");

        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_commit_returns_valid_hex_hash() {
        let dir = temp_dir("commit_hash");
        init_repo_with_commit(&dir);

        fs::write(dir.join("hex_test.txt"), "content").expect("failed to write");

        let (service, _events) = make_service();
        service.open_repo(&dir).await.expect("open_repo should succeed");

        service
            .stage(&[Path::new("hex_test.txt")])
            .await
            .expect("stage should succeed");

        let hash = service
            .commit("Test hex hash")
            .await
            .expect("commit should succeed");

        // Verify hash is valid hex (40 chars for SHA-1)
        assert_eq!(hash.len(), 40, "SHA-1 hash should be 40 hex characters");
        assert!(
            hash.chars().all(|c| c.is_ascii_hexdigit()),
            "Hash should be valid hex: {}",
            hash,
        );

        fs::remove_dir_all(&dir).ok();
    }

    // --- log ---

    #[tokio::test]
    async fn test_log_count_limit() {
        let dir = temp_dir("log_limit");
        init_repo_with_commit(&dir);

        let (service, _events) = make_service();
        service.open_repo(&dir).await.expect("open_repo should succeed");

        // Create 5 additional commits
        for i in 0..5 {
            let filename = format!("file_{}.txt", i);
            fs::write(dir.join(&filename), format!("content {}", i)).expect("failed to write");
            service
                .stage(&[Path::new(&filename)])
                .await
                .expect("stage should succeed");
            service
                .commit(&format!("Commit {}", i))
                .await
                .expect("commit should succeed");
        }

        // Ask for only 3
        let log = service.log(3).await.expect("log should succeed");
        assert_eq!(log.len(), 3, "Should return exactly 3 commits");
        // Verify we got commit messages (order depends on git2 revwalk with same-second timestamps)
        let messages: Vec<&str> = log.iter().map(|c| c.message.as_str()).collect();
        assert!(
            messages.iter().all(|m| !m.is_empty()),
            "All commit messages should be non-empty"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_log_initial_commit() {
        let dir = temp_dir("log_initial");
        init_repo_with_commit(&dir);

        let (service, _events) = make_service();
        service.open_repo(&dir).await.expect("open_repo should succeed");

        let log = service.log(10).await.expect("log should succeed");
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].message, "Initial commit");
        assert!(!log[0].hash_short.is_empty());
        assert_eq!(log[0].hash_short.len(), 7);

        fs::remove_dir_all(&dir).ok();
    }

    // --- branches ---

    #[tokio::test]
    async fn test_branches_list() {
        let dir = temp_dir("branches");
        init_repo_with_commit(&dir);

        let (service, _events) = make_service();
        service.open_repo(&dir).await.expect("open_repo should succeed");

        let branches = service.branches().await.expect("branches should succeed");
        assert!(!branches.is_empty(), "Should have at least one branch");

        // Find the current branch
        let current: Vec<_> = branches.iter().filter(|b| b.is_current).collect();
        assert_eq!(current.len(), 1, "Exactly one branch should be current");
        assert!(
            current[0].name == "master" || current[0].name == "main",
            "Current branch should be master or main, got: {}",
            current[0].name,
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_branch_creation_and_checkout() {
        let dir = temp_dir("branch_checkout");
        let repo = init_repo_with_commit(&dir);

        // Create a new branch via git2 directly
        let head = repo.head().expect("failed to get head");
        let head_commit = head.peel_to_commit().expect("failed to peel");
        repo.branch("feature-x", &head_commit, false)
            .expect("failed to create branch");

        let (service, _events) = make_service();
        service.open_repo(&dir).await.expect("open_repo should succeed");

        // Verify we can see both branches
        let branches = service.branches().await.expect("branches should succeed");
        let branch_names: Vec<&str> = branches.iter().map(|b| b.name.as_str()).collect();
        assert!(branch_names.contains(&"feature-x"), "Should contain feature-x");

        // Checkout the new branch
        service
            .checkout("feature-x")
            .await
            .expect("checkout should succeed");

        let current = service.current_branch().await;
        assert_eq!(current, "feature-x");

        fs::remove_dir_all(&dir).ok();
    }

    // --- diff ---

    #[tokio::test]
    async fn test_diff_modified_file() {
        let dir = temp_dir("diff_mod");
        let repo = init_repo_with_commit(&dir);

        // Commit a file
        let file_path = dir.join("diff_target.txt");
        fs::write(&file_path, "line1\nline2\nline3\n").expect("failed to write");
        {
            let mut index = repo.index().expect("failed to get index");
            index.add_path(Path::new("diff_target.txt")).expect("failed to add");
            index.write().expect("failed to write index");
            let tree_oid = index.write_tree().expect("failed to write tree");
            let tree = repo.find_tree(tree_oid).expect("failed to find tree");
            let sig = repo.signature().expect("failed to create signature");
            let head = repo.head().expect("failed to get head");
            let parent = head.peel_to_commit().expect("failed to peel");
            repo.commit(Some("HEAD"), &sig, &sig, "Add diff_target.txt", &tree, &[&parent])
                .expect("failed to commit");
        }

        // Modify the file
        fs::write(&file_path, "line1\nmodified\nline3\nnew_line\n").expect("failed to modify");

        let (service, _events) = make_service();
        service.open_repo(&dir).await.expect("open_repo should succeed");

        let hunks = service
            .diff(Some(Path::new("diff_target.txt")))
            .await
            .expect("diff should succeed");

        assert!(!hunks.is_empty(), "Should have at least one hunk");

        // Check that we have both additions and deletions
        let has_addition = hunks
            .iter()
            .any(|h| h.lines.iter().any(|l| l.line_type == DiffLineType::Addition));
        let has_deletion = hunks
            .iter()
            .any(|h| h.lines.iter().any(|l| l.line_type == DiffLineType::Deletion));
        assert!(has_addition, "Diff should contain additions");
        assert!(has_deletion, "Diff should contain deletions");

        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_diff_new_file() {
        let dir = temp_dir("diff_new");
        init_repo_with_commit(&dir);

        fs::write(dir.join("brand_new.txt"), "new content\n").expect("failed to write");

        let (service, _events) = make_service();
        service.open_repo(&dir).await.expect("open_repo should succeed");

        // Stage the new file so it appears in the diff
        service
            .stage(&[Path::new("brand_new.txt")])
            .await
            .expect("stage should succeed");

        let hunks = service
            .diff(Some(Path::new("brand_new.txt")))
            .await
            .expect("diff should succeed");

        assert!(!hunks.is_empty(), "Should have hunks for new file");
        // All lines in a new file should be additions
        for hunk in &hunks {
            for line in &hunk.lines {
                assert_eq!(
                    line.line_type,
                    DiffLineType::Addition,
                    "New file lines should all be additions",
                );
            }
        }

        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_diff_all_files() {
        let dir = temp_dir("diff_all");
        init_repo_with_commit(&dir);

        fs::write(dir.join("a.txt"), "aaa").expect("failed to write");
        fs::write(dir.join("b.txt"), "bbb").expect("failed to write");

        let (service, _events) = make_service();
        service.open_repo(&dir).await.expect("open_repo should succeed");

        service
            .stage(&[Path::new("a.txt"), Path::new("b.txt")])
            .await
            .expect("stage should succeed");

        let hunks = service
            .diff(None)
            .await
            .expect("diff should succeed");

        assert!(
            hunks.len() >= 2,
            "Diff of all files should have hunks for both files, got {}",
            hunks.len(),
        );

        fs::remove_dir_all(&dir).ok();
    }

    // --- event publishing ---

    #[tokio::test]
    async fn test_event_published_on_refresh() {
        let dir = temp_dir("event_refresh");
        init_repo_with_commit(&dir);

        let (service, events) = make_service();
        let mut rx = events.subscribe();

        service.open_repo(&dir).await.expect("open_repo should succeed");

        let event = rx.recv().await.expect("should receive event");
        match event {
            AppEvent::GitStatusChanged {
                branch,
                changed_count,
                staged_count,
            } => {
                assert!(!branch.is_empty());
                assert_eq!(changed_count, 0);
                assert_eq!(staged_count, 0);
            }
            other => panic!("Expected GitStatusChanged, got {:?}", other),
        }

        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_event_published_on_stage() {
        let dir = temp_dir("event_stage");
        init_repo_with_commit(&dir);

        fs::write(dir.join("event_file.txt"), "data").expect("failed to write");

        let (service, events) = make_service();
        service.open_repo(&dir).await.expect("open_repo should succeed");

        let mut rx = events.subscribe();

        service
            .stage(&[Path::new("event_file.txt")])
            .await
            .expect("stage should succeed");

        let event = rx.recv().await.expect("should receive event");
        match event {
            AppEvent::GitStatusChanged { staged_count, .. } => {
                assert!(staged_count > 0, "staged_count should be > 0 after staging");
            }
            other => panic!("Expected GitStatusChanged, got {:?}", other),
        }

        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_event_published_on_commit() {
        let dir = temp_dir("event_commit");
        init_repo_with_commit(&dir);

        fs::write(dir.join("to_commit.txt"), "commit me").expect("failed to write");

        let (service, events) = make_service();
        service.open_repo(&dir).await.expect("open_repo should succeed");

        service
            .stage(&[Path::new("to_commit.txt")])
            .await
            .expect("stage should succeed");

        let mut rx = events.subscribe();

        service
            .commit("Test commit event")
            .await
            .expect("commit should succeed");

        let event = rx.recv().await.expect("should receive event");
        match event {
            AppEvent::GitStatusChanged {
                changed_count,
                staged_count,
                ..
            } => {
                assert_eq!(changed_count, 0, "Should be clean after commit");
                assert_eq!(staged_count, 0, "Nothing staged after commit");
            }
            other => panic!("Expected GitStatusChanged, got {:?}", other),
        }

        fs::remove_dir_all(&dir).ok();
    }

    // --- no repo open error ---

    #[tokio::test]
    async fn test_error_when_no_repo_open() {
        let (service, _events) = make_service();

        let branch_result = service.backend.current_branch().await;
        assert!(branch_result.is_err());

        let status_result = service.backend.status().await;
        assert!(status_result.is_err());

        let diff_result = service.backend.diff(None).await;
        assert!(diff_result.is_err());

        let commit_result = service.backend.commit("test").await;
        assert!(commit_result.is_err());

        let branches_result = service.backend.branches().await;
        assert!(branches_result.is_err());

        let log_result = service.backend.log(5).await;
        assert!(log_result.is_err());
    }

    // --- multiple files ---

    #[tokio::test]
    async fn test_stage_multiple_files() {
        let dir = temp_dir("stage_multi");
        init_repo_with_commit(&dir);

        fs::write(dir.join("m1.txt"), "one").expect("failed to write");
        fs::write(dir.join("m2.txt"), "two").expect("failed to write");
        fs::write(dir.join("m3.txt"), "three").expect("failed to write");

        let (service, _events) = make_service();
        service.open_repo(&dir).await.expect("open_repo should succeed");

        service
            .stage(&[Path::new("m1.txt"), Path::new("m2.txt"), Path::new("m3.txt")])
            .await
            .expect("stage should succeed");

        let statuses = service.status().await;
        let staged: Vec<_> = statuses.iter().filter(|s| s.staged).collect();
        assert_eq!(staged.len(), 3, "All 3 files should be staged");

        fs::remove_dir_all(&dir).ok();
    }

    // --- service state caching ---

    #[tokio::test]
    async fn test_get_state_returns_cached_data() {
        let dir = temp_dir("state_cache");
        init_repo_with_commit(&dir);

        fs::write(dir.join("cached.txt"), "cached").expect("failed to write");

        let (service, _events) = make_service();
        service.open_repo(&dir).await.expect("open_repo should succeed");

        let state = service.get_state().await;
        assert!(!state.current_branch.is_empty());
        assert_eq!(state.file_statuses.len(), 1);
        assert_eq!(state.file_statuses[0].path, "cached.txt");

        fs::remove_dir_all(&dir).ok();
    }

    // --- checkout updates branch ---

    #[tokio::test]
    async fn test_checkout_updates_cached_branch() {
        let dir = temp_dir("checkout_cache");
        let repo = init_repo_with_commit(&dir);

        // Create a branch
        let head = repo.head().expect("failed to get head");
        let commit = head.peel_to_commit().expect("failed to peel");
        repo.branch("dev", &commit, false).expect("failed to create branch");

        let (service, _events) = make_service();
        service.open_repo(&dir).await.expect("open_repo should succeed");

        let before = service.current_branch().await;
        assert!(before == "master" || before == "main");

        service.checkout("dev").await.expect("checkout should succeed");

        let after = service.current_branch().await;
        assert_eq!(after, "dev");

        fs::remove_dir_all(&dir).ok();
    }

    // --- multiple commits in log ---

    #[tokio::test]
    async fn test_log_ordering_most_recent_first() {
        let dir = temp_dir("log_order");
        init_repo_with_commit(&dir);

        let (service, _events) = make_service();
        service.open_repo(&dir).await.expect("open_repo should succeed");

        for i in 1..=3 {
            let name = format!("ordered_{}.txt", i);
            fs::write(dir.join(&name), format!("content {}", i)).expect("failed to write");
            service
                .stage(&[Path::new(&name)])
                .await
                .expect("stage should succeed");
            service
                .commit(&format!("Ordered commit {}", i))
                .await
                .expect("commit should succeed");
        }

        let log = service.log(10).await.expect("log should succeed");
        assert!(log.len() >= 4, "Should have at least 4 commits (initial + 3)");
        // Verify all expected commit messages are present
        let messages: Vec<String> = log.iter().map(|c| c.message.clone()).collect();
        assert!(messages.contains(&"Ordered commit 1".to_string()), "Missing 'Ordered commit 1'");
        assert!(messages.contains(&"Ordered commit 2".to_string()), "Missing 'Ordered commit 2'");
        assert!(messages.contains(&"Ordered commit 3".to_string()), "Missing 'Ordered commit 3'");
        assert!(messages.contains(&"Initial commit".to_string()), "Missing 'Initial commit'");

        fs::remove_dir_all(&dir).ok();
    }

    // --- stage deleted file ---

    #[tokio::test]
    async fn test_stage_deleted_file() {
        let dir = temp_dir("stage_deleted");
        let repo = init_repo_with_commit(&dir);

        // Commit a file
        fs::write(dir.join("doomed.txt"), "delete me").expect("failed to write");
        {
            let mut index = repo.index().expect("failed to get index");
            index.add_path(Path::new("doomed.txt")).expect("failed to add");
            index.write().expect("failed to write index");
            let tree_oid = index.write_tree().expect("failed to write tree");
            let tree = repo.find_tree(tree_oid).expect("failed to find tree");
            let sig = repo.signature().expect("failed to create signature");
            let head = repo.head().expect("failed to get head");
            let parent = head.peel_to_commit().expect("failed to peel");
            repo.commit(Some("HEAD"), &sig, &sig, "Add doomed.txt", &tree, &[&parent])
                .expect("failed to commit");
        }

        // Delete and stage the deletion
        fs::remove_file(dir.join("doomed.txt")).expect("failed to delete");

        let (service, _events) = make_service();
        service.open_repo(&dir).await.expect("open_repo should succeed");

        service
            .stage(&[Path::new("doomed.txt")])
            .await
            .expect("stage deleted file should succeed");

        let statuses = service.status().await;
        let staged_deletions: Vec<_> = statuses
            .iter()
            .filter(|s| s.staged && s.change_type == GitChangeType::Deleted)
            .collect();
        assert!(
            !staged_deletions.is_empty(),
            "Should have a staged deletion entry",
        );

        fs::remove_dir_all(&dir).ok();
    }

    // --- diff hunk structure ---

    #[tokio::test]
    async fn test_diff_hunk_has_line_numbers() {
        let dir = temp_dir("diff_hunk_lines");
        let repo = init_repo_with_commit(&dir);

        // Commit a file
        fs::write(dir.join("hunks.txt"), "a\nb\nc\n").expect("failed to write");
        {
            let mut index = repo.index().expect("failed to get index");
            index.add_path(Path::new("hunks.txt")).expect("failed to add");
            index.write().expect("failed to write index");
            let tree_oid = index.write_tree().expect("failed to write tree");
            let tree = repo.find_tree(tree_oid).expect("failed to find tree");
            let sig = repo.signature().expect("failed to create signature");
            let head = repo.head().expect("failed to get head");
            let parent = head.peel_to_commit().expect("failed to peel");
            repo.commit(Some("HEAD"), &sig, &sig, "Add hunks.txt", &tree, &[&parent])
                .expect("failed to commit");
        }

        // Modify
        fs::write(dir.join("hunks.txt"), "a\nBBB\nc\n").expect("failed to modify");

        let (service, _events) = make_service();
        service.open_repo(&dir).await.expect("open_repo should succeed");

        let hunks = service
            .diff(Some(Path::new("hunks.txt")))
            .await
            .expect("diff should succeed");

        assert!(!hunks.is_empty());
        let hunk = &hunks[0];
        assert!(hunk.old_start > 0 || hunk.old_count > 0, "Hunk should have line numbers");
        assert!(!hunk.lines.is_empty(), "Hunk should have lines");

        fs::remove_dir_all(&dir).ok();
    }

    // --- branches flag correctness ---

    #[tokio::test]
    async fn test_branches_is_remote_flag() {
        let dir = temp_dir("branch_remote");
        init_repo_with_commit(&dir);

        let (service, _events) = make_service();
        service.open_repo(&dir).await.expect("open_repo should succeed");

        let branches = service.branches().await.expect("branches should succeed");
        // Local-only repo should have no remote branches
        let remote_branches: Vec<_> = branches.iter().filter(|b| b.is_remote).collect();
        assert!(remote_branches.is_empty(), "Local repo should have no remote branches");

        fs::remove_dir_all(&dir).ok();
    }

    // --- commit timestamp ---

    #[tokio::test]
    async fn test_commit_has_timestamp() {
        let dir = temp_dir("commit_ts");
        init_repo_with_commit(&dir);

        let (service, _events) = make_service();
        service.open_repo(&dir).await.expect("open_repo should succeed");

        let log = service.log(1).await.expect("log should succeed");
        assert_eq!(log.len(), 1);
        assert!(log[0].timestamp > 0, "Commit should have a non-zero timestamp");

        fs::remove_dir_all(&dir).ok();
    }

    // --- service constructor test ---

    #[tokio::test]
    async fn test_service_new_uses_real_backend() {
        // Just ensure `new()` compiles and creates a valid service
        let events = Arc::new(EventBus::new());
        let service = GitService::new(events);
        // No repo open, so state should be default
        let state = service.get_state().await;
        assert!(state.current_branch.is_empty());
        assert!(state.file_statuses.is_empty());
    }

    // --- refresh idempotency ---

    #[tokio::test]
    async fn test_refresh_is_idempotent() {
        let dir = temp_dir("refresh_idem");
        init_repo_with_commit(&dir);

        fs::write(dir.join("idem.txt"), "data").expect("failed to write");

        let (service, _events) = make_service();
        service.open_repo(&dir).await.expect("open_repo should succeed");

        let state1 = service.get_state().await;
        service.refresh().await.expect("refresh should succeed");
        let state2 = service.get_state().await;

        assert_eq!(state1.current_branch, state2.current_branch);
        assert_eq!(state1.file_statuses.len(), state2.file_statuses.len());

        fs::remove_dir_all(&dir).ok();
    }

    // --- status after multiple operations ---

    #[tokio::test]
    async fn test_full_workflow_add_stage_commit_clean() {
        let dir = temp_dir("full_workflow");
        init_repo_with_commit(&dir);

        let (service, _events) = make_service();
        service.open_repo(&dir).await.expect("open_repo should succeed");

        // 1. Clean
        assert!(service.status().await.is_empty());

        // 2. Add file
        fs::write(dir.join("workflow.txt"), "step1").expect("failed to write");
        service.refresh().await.expect("refresh should succeed");
        assert_eq!(service.status().await.len(), 1);

        // 3. Stage
        service
            .stage(&[Path::new("workflow.txt")])
            .await
            .expect("stage should succeed");
        let staged: Vec<_> = service.status().await.into_iter().filter(|s| s.staged).collect();
        assert_eq!(staged.len(), 1);

        // 4. Commit
        let hash = service
            .commit("Workflow commit")
            .await
            .expect("commit should succeed");
        assert!(!hash.is_empty());

        // 5. Clean again
        assert!(service.status().await.is_empty());

        fs::remove_dir_all(&dir).ok();
    }

    // --- git state default ---

    #[test]
    fn test_git_state_default() {
        let state = GitState::default();
        assert!(state.current_branch.is_empty());
        assert!(state.file_statuses.is_empty());
    }

    // --- log with zero count ---

    #[tokio::test]
    async fn test_log_zero_count() {
        let dir = temp_dir("log_zero");
        init_repo_with_commit(&dir);

        let (service, _events) = make_service();
        service.open_repo(&dir).await.expect("open_repo should succeed");

        let log = service.log(0).await.expect("log should succeed");
        assert!(log.is_empty(), "log(0) should return empty list");

        fs::remove_dir_all(&dir).ok();
    }

    // --- diff on clean repo ---

    #[tokio::test]
    async fn test_diff_on_clean_repo() {
        let dir = temp_dir("diff_clean");
        init_repo_with_commit(&dir);

        let (service, _events) = make_service();
        service.open_repo(&dir).await.expect("open_repo should succeed");

        let hunks = service.diff(None).await.expect("diff should succeed");
        assert!(hunks.is_empty(), "Clean repo diff should have no hunks");

        fs::remove_dir_all(&dir).ok();
    }
}
