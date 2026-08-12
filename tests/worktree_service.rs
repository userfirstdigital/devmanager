#[cfg(test)]
#[macro_export]
macro_rules! worktree_service_focused_tests {
    () => {
        use $crate::workspace::worktree::{
            parse_worktree_porcelain, sealed, AddResult, CancellationToken, CleanupConfirmation,
            CleanupSnapshot, CleanupState, CreateWorktreeRequest, DurableOperationJournal,
            ExecutionBudget, ExecutorError, GitWorktreeExecutor, JournalContext, JournalError,
            JournalKind, JournalOperation, JournalState, OperationKey, ProbeResult,
            ProcessZeroProof, RecoveryLookup, ResolvedWorkspace, SqliteTestJournal,
            SqliteWorktreeJournal, TestGitWorktreeExecutor, TestOperationJournal,
            TestWorkspaceAuthorization, TestWorkspaceControl, WorktreeError, WorktreeHold,
            WorktreeJournalStore, WorktreePlan, WorktreeService, WorktreeTarget,
            MAX_JOURNAL_OPERATIONS, MAX_PORCELAIN_BYTES,
        };

        use sha2::{Digest, Sha256};
        use std::collections::BTreeMap;
        use std::ffi::OsString;
        use std::io::Read;
        use std::path::{Path, PathBuf};
        use std::process::{Command, Output, Stdio};
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        use std::sync::{Arc, Mutex};
        use std::thread;
        use std::time::Duration;

        #[derive(Clone)]
        struct RealGitWorktreeExecutor {
            repository: PathBuf,
            entries: Arc<Mutex<BTreeMap<[u8; 16], RealGitEntry>>>,
            zero_observation: Arc<AtomicU64>,
            interrupt_after_add: Arc<AtomicBool>,
            active_children: Arc<AtomicU64>,
        }

        #[derive(Clone)]
        struct RealGitEntry {
            path: PathBuf,
            base_revision: String,
            gitdir_fingerprint: [u8; 32],
            gitdir_identity: [u8; 32],
            gitdir_handle: Arc<std::fs::File>,
            receipt: $crate::workspace::worktree::CreatedWorktree,
        }

        struct ActiveChildGuard(Arc<AtomicU64>);

        impl Drop for ActiveChildGuard {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::AcqRel);
            }
        }

        struct OwnedChild(std::process::Child);

        impl std::ops::Deref for OwnedChild {
            type Target = std::process::Child;

            fn deref(&self) -> &Self::Target {
                &self.0
            }
        }

        impl std::ops::DerefMut for OwnedChild {
            fn deref_mut(&mut self) -> &mut Self::Target {
                &mut self.0
            }
        }

        impl OwnedChild {
            fn terminate(&mut self) {
                // `git.exe daemon` is a launcher on Windows: it keeps a
                // `git-daemon.exe` child that inherits the stdout/stderr pipe.
                // Killing only the launcher leaves the reader joins blocked
                // forever, so cancellation must own and reap the whole exact
                // child tree before joining either reader.
                #[cfg(windows)]
                {
                    let pid = self.0.id().to_string();
                    let _ = std::process::Command::new("taskkill")
                        .args(["/PID", pid.as_str(), "/T", "/F"])
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                }
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        impl Drop for OwnedChild {
            fn drop(&mut self) {
                self.terminate();
            }
        }

        impl RealGitWorktreeExecutor {
            fn new(repository: impl Into<PathBuf>) -> Self {
                Self {
                    repository: repository.into(),
                    entries: Arc::new(Mutex::new(BTreeMap::new())),
                    zero_observation: Arc::new(AtomicU64::new(0)),
                    interrupt_after_add: Arc::new(AtomicBool::new(false)),
                    active_children: Arc::new(AtomicU64::new(0)),
                }
            }

            fn interrupt_after_add(&self, value: bool) {
                self.interrupt_after_add.store(value, Ordering::SeqCst);
            }

            fn path_for(&self, operation: JournalOperation) -> PathBuf {
                let suffix = operation
                    .key
                    .0
                    .iter()
                    .take(8)
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>();
                self.repository.join(format!("linked-{suffix}"))
            }

            fn linked_path(&self, operation: JournalOperation) -> PathBuf {
                self.path_for(operation)
            }

            fn target_for(&self, operation: JournalOperation) -> WorktreeTarget {
                WorktreeTarget::for_test(self.repository.clone(), self.path_for(operation))
            }

            fn check(
                cancellation: &CancellationToken,
                budget: $crate::workspace::worktree::ExecutionBudget,
            ) -> Result<(), ExecutorError> {
                if cancellation.is_cancelled() {
                    Err(ExecutorError::Cancelled)
                } else if budget.expired() {
                    Err(ExecutorError::Deadline)
                } else {
                    Ok(())
                }
            }

            fn read_bounded<R: Read>(
                mut reader: R,
                limit: usize,
            ) -> Result<(Vec<u8>, bool), ExecutorError> {
                let mut output = Vec::with_capacity(limit.min(8192));
                let mut buffer = [0u8; 8192];
                let mut overflow = false;
                loop {
                    let count = match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Err(_) => return Err(ExecutorError::CompensationFailed),
                        Ok(count) => count,
                    };
                    let remaining = limit.saturating_sub(output.len());
                    overflow |= count > remaining;
                    output.extend_from_slice(&buffer[..count.min(remaining)]);
                }
                Ok((output, overflow))
            }

            fn git(
                &self,
                args: Vec<OsString>,
                cancellation: &CancellationToken,
                budget: $crate::workspace::worktree::ExecutionBudget,
            ) -> Result<Output, ExecutorError> {
                self.git_at(&self.repository, args, cancellation, budget)
            }

            fn git_at(
                &self,
                directory: &Path,
                args: Vec<OsString>,
                cancellation: &CancellationToken,
                budget: $crate::workspace::worktree::ExecutionBudget,
            ) -> Result<Output, ExecutorError> {
                Self::check(cancellation, budget)?;
                self.active_children.fetch_add(1, Ordering::AcqRel);
                let _active_child = ActiveChildGuard(Arc::clone(&self.active_children));
                let result = (|| {
                    let mut child = OwnedChild(
                        Command::new("git")
                            .args(args)
                            .env("GIT_TERMINAL_PROMPT", "0")
                            .stdin(Stdio::null())
                            .stdout(Stdio::piped())
                            .stderr(Stdio::piped())
                            .current_dir(directory)
                            .spawn()
                            .map_err(|_| ExecutorError::CompensationFailed)?,
                    );
                    let stdout = match child.stdout.take() {
                        Some(stdout) => stdout,
                        None => {
                            child.terminate();
                            return Err(ExecutorError::CompensationFailed);
                        }
                    };
                    let stderr = match child.stderr.take() {
                        Some(stderr) => stderr,
                        None => {
                            child.terminate();
                            return Err(ExecutorError::CompensationFailed);
                        }
                    };
                    let output_limit = budget.max_output_bytes();
                    let stdout_thread =
                        thread::spawn(move || Self::read_bounded(stdout, output_limit));
                    let output_limit = budget.max_output_bytes();
                    let stderr_thread =
                        thread::spawn(move || Self::read_bounded(stderr, output_limit));
                    let status = loop {
                        if cancellation.is_cancelled() || budget.expired() {
                            child.terminate();
                            let _ = stdout_thread.join();
                            let _ = stderr_thread.join();
                            return Err(if cancellation.is_cancelled() {
                                ExecutorError::Cancelled
                            } else {
                                ExecutorError::Deadline
                            });
                        }
                        match child.try_wait() {
                            Ok(Some(status)) => break status,
                            Ok(None) => {}
                            Err(_) => {
                                child.terminate();
                                let _ = stdout_thread.join();
                                let _ = stderr_thread.join();
                                return Err(ExecutorError::CompensationFailed);
                            }
                        }
                        thread::sleep(Duration::from_millis(2));
                    };
                    let (stdout, stdout_overflow) = stdout_thread
                        .join()
                        .map_err(|_| ExecutorError::CompensationFailed)??;
                    let (stderr, stderr_overflow) = stderr_thread
                        .join()
                        .map_err(|_| ExecutorError::CompensationFailed)??;
                    if stdout_overflow || stderr_overflow {
                        return Err(ExecutorError::OversizeOutput);
                    }
                    Ok(Output {
                        status,
                        stdout,
                        stderr,
                    })
                })();
                result
            }

            fn setup_git(&self, args: Vec<OsString>) -> Result<Output, ExecutorError> {
                let cancellation = CancellationToken::new();
                self.git(
                    args,
                    &cancellation,
                    $crate::workspace::worktree::ExecutionBudget::from_timeout(
                        Duration::from_secs(10),
                    ),
                )
            }

            fn commit_fingerprint(output: &Output) -> Result<[u8; 32], ExecutorError> {
                if !output.status.success() {
                    return Err(ExecutorError::CompensationFailed);
                }
                let mut hasher = Sha256::new();
                hasher.update(b"real-git-commit-v1\0");
                hasher.update(output.stdout.trim_ascii());
                Ok(hasher.finalize().into())
            }

            fn commit_revision(output: &Output) -> Result<String, ExecutorError> {
                if !output.status.success() {
                    return Err(ExecutorError::CompensationFailed);
                }
                let revision = String::from_utf8(output.stdout.clone())
                    .map_err(|_| ExecutorError::MalformedOutput)?
                    .trim()
                    .to_owned();
                if revision.is_empty()
                    || revision.len() > 128
                    || !revision.bytes().all(|byte| byte.is_ascii_hexdigit())
                {
                    return Err(ExecutorError::MalformedOutput);
                }
                Ok(revision)
            }

            fn gitdir_fingerprint(path: &std::path::Path) -> Result<[u8; 32], ExecutorError> {
                let gitdir = path.join(".git");
                let metadata = std::fs::symlink_metadata(&gitdir)
                    .map_err(|_| ExecutorError::IdentityMismatch)?;
                if metadata.file_type().is_symlink() {
                    return Err(ExecutorError::IdentityMismatch);
                }
                let bytes = std::fs::read(gitdir).map_err(|_| ExecutorError::IdentityMismatch)?;
                let mut hasher = Sha256::new();
                hasher.update(b"real-git-linked-gitdir-v1\0");
                hasher.update(bytes);
                Ok(hasher.finalize().into())
            }

            fn target_present(path: &Path) -> Result<bool, ExecutorError> {
                match std::fs::symlink_metadata(path) {
                    Ok(_) => Ok(true),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                    Err(_) => Err(ExecutorError::IdentityMismatch),
                }
            }

            fn validate_target(target: &WorktreeTarget) -> Result<(), ExecutorError> {
                if !target.validate() {
                    return Err(ExecutorError::IdentityMismatch);
                }
                let root = std::fs::symlink_metadata(&target.approved_root)
                    .map_err(|_| ExecutorError::IdentityMismatch)?;
                if !root.is_dir() || root.file_type().is_symlink() {
                    return Err(ExecutorError::IdentityMismatch);
                }
                if let Ok(metadata) = std::fs::symlink_metadata(&target.path) {
                    if metadata.file_type().is_symlink() {
                        return Err(ExecutorError::IdentityMismatch);
                    }
                }
                let mut current = target.path.clone();
                loop {
                    if std::fs::symlink_metadata(&current).is_ok()
                        && $crate::workspace::worktree::retained_path_identity(&current).is_none()
                    {
                        return Err(ExecutorError::IdentityMismatch);
                    }
                    if $crate::workspace::model::path_identity_key(&current)
                        == $crate::workspace::model::path_identity_key(&target.approved_root)
                    {
                        break;
                    }
                    if !current.pop() {
                        return Err(ExecutorError::IdentityMismatch);
                    }
                }
                Ok(())
            }

            fn unpushed_state(
                &self,
                entry: &RealGitEntry,
                cancellation: &CancellationToken,
                budget: $crate::workspace::worktree::ExecutionBudget,
            ) -> Result<CleanupState, ExecutorError> {
                // Compare against the immutable commit retained at add time.
                // Looking up a moving branch or upstream ref can compare the
                // ref to itself and silently miss a local commit made after
                // creation; cleanup must conservatively refuse that state.
                let reference = entry.base_revision.clone();
                if reference.is_empty()
                    || reference.len() > 128
                    || !reference.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_' | b'-' | b'.')
                    })
                {
                    return Err(ExecutorError::MalformedOutput);
                }
                let count = self.git_at(
                    &entry.path,
                    vec![
                        OsString::from("rev-list"),
                        OsString::from("--count"),
                        OsString::from(format!("{reference}..HEAD")),
                    ],
                    cancellation,
                    budget,
                )?;
                if !count.status.success() {
                    return Ok(CleanupState::Dirty);
                }
                let count = String::from_utf8(count.stdout)
                    .map_err(|_| ExecutorError::MalformedOutput)?
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| ExecutorError::MalformedOutput)?;
                Ok(if count == 0 {
                    CleanupState::Clean
                } else {
                    CleanupState::Dirty
                })
            }

            fn nested_state(
                &self,
                root: &Path,
                cancellation: &CancellationToken,
                budget: $crate::workspace::worktree::ExecutionBudget,
            ) -> Result<CleanupState, ExecutorError> {
                const MAX_NESTED_DEPTH: u8 = 8;
                let mut directories = vec![(root.to_path_buf(), 0u8)];
                let mut nodes = 0usize;
                let mut scanned_name_bytes = 0usize;
                while let Some((directory, depth)) = directories.pop() {
                    Self::check(cancellation, budget)?;
                    if depth > 0 && std::fs::symlink_metadata(directory.join(".git")).is_ok() {
                        return Ok(CleanupState::Dirty);
                    }
                    let entries = std::fs::read_dir(&directory)
                        .map_err(|_| ExecutorError::IdentityMismatch)?;
                    for entry in entries {
                        Self::check(cancellation, budget)?;
                        nodes = nodes.saturating_add(1);
                        if nodes > budget.max_nodes() {
                            return Err(ExecutorError::OversizeOutput);
                        }
                        let entry = entry.map_err(|_| ExecutorError::IdentityMismatch)?;
                        scanned_name_bytes =
                            scanned_name_bytes.saturating_add(entry.file_name().len());
                        if scanned_name_bytes > budget.max_bytes() {
                            return Err(ExecutorError::OversizeOutput);
                        }
                        let file_type = entry
                            .file_type()
                            .map_err(|_| ExecutorError::IdentityMismatch)?;
                        if file_type.is_symlink() {
                            return Ok(CleanupState::Dirty);
                        }
                        if !file_type.is_dir() {
                            continue;
                        }
                        let name = entry.file_name();
                        if name == ".git" {
                            continue;
                        }
                        if depth >= MAX_NESTED_DEPTH {
                            // The scan is deliberately bounded; an
                            // unexamined directory is uncertainty, never a
                            // reason to delete the worktree.
                            return Ok(CleanupState::Dirty);
                        }
                        directories.push((entry.path(), depth.saturating_add(1)));
                    }
                }
                Ok(CleanupState::Clean)
            }

            fn worktree_membership(
                &self,
                entry: &RealGitEntry,
                cancellation: &CancellationToken,
                budget: $crate::workspace::worktree::ExecutionBudget,
            ) -> Result<(CleanupState, CleanupState, CleanupState), ExecutorError> {
                let listing = self.git(
                    vec![
                        OsString::from("worktree"),
                        OsString::from("list"),
                        OsString::from("--porcelain"),
                        OsString::from("-z"),
                    ],
                    cancellation,
                    budget,
                )?;
                if !listing.status.success() {
                    return Err(ExecutorError::MalformedOutput);
                }
                $crate::workspace::worktree::parse_worktree_porcelain(&listing.stdout).map_err(
                    |error| match error {
                        $crate::workspace::worktree::PorcelainError::Oversize
                        | $crate::workspace::worktree::PorcelainError::TooManyRecords => {
                            ExecutorError::OversizeOutput
                        }
                        $crate::workspace::worktree::PorcelainError::Malformed => {
                            ExecutorError::MalformedOutput
                        }
                    },
                )?;

                let mut current_path = None::<PathBuf>;
                let mut target_seen = 0usize;
                let mut root_seen = 0usize;
                let mut target_branch_matches = false;
                let expected_branch = format!("refs/heads/{}", entry.receipt.branch);
                let same_path = |left: &Path, right: &Path| {
                    // Git's porcelain uses `/` even on Windows while the
                    // retained authority may carry `\\` separators. Compare
                    // the canonical identities, not their display spelling;
                    // this also prevents a textual separator/case mismatch
                    // from making an approved linked worktree look foreign.
                    let Ok(left) = std::fs::canonicalize(left) else {
                        return false;
                    };
                    let Ok(right) = std::fs::canonicalize(right) else {
                        return false;
                    };
                    if cfg!(windows) {
                        left.to_string_lossy()
                            .eq_ignore_ascii_case(&right.to_string_lossy())
                    } else {
                        left == right
                    }
                };
                for field in listing.stdout.split(|byte| *byte == 0) {
                    Self::check(cancellation, budget)?;
                    if let Some(value) = field.strip_prefix(b"worktree ") {
                        let value = String::from_utf8(value.to_vec())
                            .map_err(|_| ExecutorError::MalformedOutput)?;
                        let path = PathBuf::from(value);
                        if same_path(&path, &entry.path) {
                            target_seen = target_seen.saturating_add(1);
                        }
                        if same_path(&path, &self.repository) {
                            root_seen = root_seen.saturating_add(1);
                        }
                        current_path = Some(path);
                    } else if let Some(value) = field.strip_prefix(b"branch ") {
                        if current_path
                            .as_ref()
                            .is_some_and(|path| same_path(path, &entry.path))
                            && value == expected_branch.as_bytes()
                        {
                            target_branch_matches = true;
                        }
                    } else if field.is_empty() {
                        current_path = None;
                    }
                }
                if target_seen != 1 || root_seen != 1 {
                    return Err(ExecutorError::IdentityMismatch);
                }
                let target_is_main = same_path(&entry.path, &self.repository);
                let linked = if target_is_main {
                    CleanupState::Dirty
                } else {
                    CleanupState::Clean
                };
                let foreign = if target_branch_matches {
                    CleanupState::Clean
                } else {
                    CleanupState::Dirty
                };
                let main_checkout = if target_is_main {
                    CleanupState::Dirty
                } else {
                    CleanupState::Clean
                };
                Ok((linked, foreign, main_checkout))
            }

            fn validate_entry(&self, entry: &RealGitEntry) -> Result<(), ExecutorError> {
                let metadata = std::fs::symlink_metadata(&entry.path)
                    .map_err(|_| ExecutorError::IdentityMismatch)?;
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    return Err(ExecutorError::IdentityMismatch);
                }
                if Self::gitdir_fingerprint(&entry.path)? != entry.gitdir_fingerprint {
                    return Err(ExecutorError::IdentityMismatch);
                }
                let current_gitdir = std::fs::File::open(entry.path.join(".git"))
                    .map_err(|_| ExecutorError::IdentityMismatch)?;
                if $crate::workspace::worktree::retained_file_identity(&current_gitdir)
                    != Some(entry.gitdir_identity)
                {
                    return Err(ExecutorError::IdentityMismatch);
                }
                let gitdir_metadata = entry
                    .gitdir_handle
                    .metadata()
                    .map_err(|_| ExecutorError::IdentityMismatch)?;
                if gitdir_metadata.len() == 0 {
                    return Err(ExecutorError::IdentityMismatch);
                }
                Ok(())
            }

            fn entry(&self, operation: JournalOperation) -> Option<RealGitEntry> {
                self.entries
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .get(&operation.key.0)
                    .cloned()
            }

            fn remove_entry(&self, operation: JournalOperation) {
                self.entries
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&operation.key.0);
            }

            fn active_child_count(&self) -> u64 {
                self.active_children.load(Ordering::Acquire)
            }

            fn add_entry(&self, operation: JournalOperation, entry: RealGitEntry) {
                self.entries
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(operation.key.0, entry);
            }
        }

        impl sealed::Executor for RealGitWorktreeExecutor {}

        impl GitWorktreeExecutor for RealGitWorktreeExecutor {
            fn probe(
                &self,
                _workspace: &ResolvedWorkspace,
                plan: &WorktreePlan,
                cancellation: &CancellationToken,
                budget: $crate::workspace::worktree::ExecutionBudget,
            ) -> Result<ProbeResult, ExecutorError> {
                Self::check(cancellation, budget)?;
                if plan.target.validate() {
                    Self::validate_target(&plan.target)?;
                }
                let reference = OsString::from(format!("refs/heads/{}", plan.branch));
                let branch = self.git(
                    vec![
                        OsString::from("show-ref"),
                        OsString::from("--verify"),
                        OsString::from("--quiet"),
                        reference,
                    ],
                    cancellation,
                    budget,
                )?;
                if branch.status.success() {
                    return Ok(ProbeResult::Collision);
                }
                Self::check(cancellation, budget)?;
                let head = self.git(
                    vec![OsString::from("rev-parse"), OsString::from("HEAD")],
                    cancellation,
                    budget,
                )?;
                let base_revision = Self::commit_revision(&head)?;
                let target = if plan.target.validate() {
                    plan.target.clone()
                } else {
                    let suffix = plan
                        .identity
                        .0
                        .iter()
                        .take(8)
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>();
                    $crate::workspace::worktree::WorktreeTarget::for_test(
                        self.repository.clone(),
                        self.repository.join(format!("linked-{suffix}")),
                    )
                };
                Ok(ProbeResult::Available {
                    base_commit: Self::commit_fingerprint(&head)?,
                    base_revision,
                    target,
                })
            }

            fn add(
                &self,
                _workspace: &ResolvedWorkspace,
                operation: JournalOperation,
                plan: &WorktreePlan,
                cancellation: &CancellationToken,
                budget: $crate::workspace::worktree::ExecutionBudget,
            ) -> Result<AddResult, ExecutorError> {
                Self::check(cancellation, budget)?;
                let base_revision = plan.base_revision.clone();
                if !plan.target.validate() {
                    return Err(ExecutorError::IdentityMismatch);
                }
                Self::validate_target(&plan.target)?;
                let path = plan.target.path.clone();
                if Self::target_present(&path)? {
                    return Err(ExecutorError::Collision);
                }
                let output = self.git(
                    vec![
                        OsString::from("worktree"),
                        OsString::from("add"),
                        OsString::from("--quiet"),
                        OsString::from("-b"),
                        OsString::from(plan.branch.as_str()),
                        path.as_os_str().to_owned(),
                        OsString::from(base_revision.as_str()),
                    ],
                    cancellation,
                    budget,
                )?;
                if !output.status.success() {
                    return Err(ExecutorError::Collision);
                }
                let created_head = Self::commit_revision(&self.git_at(
                    &path,
                    vec![OsString::from("rev-parse"), OsString::from("HEAD")],
                    cancellation,
                    budget,
                )?)?;
                if created_head != base_revision {
                    return Err(ExecutorError::IdentityMismatch);
                }
                let gitdir_fingerprint = Self::gitdir_fingerprint(&path)?;
                let gitdir_handle = Arc::new(
                    std::fs::File::open(path.join(".git"))
                        .map_err(|_| ExecutorError::IdentityMismatch)?,
                );
                let gitdir_identity =
                    $crate::workspace::worktree::retained_file_identity(&gitdir_handle)
                        .ok_or(ExecutorError::IdentityMismatch)?;
                Self::check(cancellation, budget)?;
                let receipt = $crate::workspace::worktree::CreatedWorktree {
                    operation_id: operation.key,
                    scope: plan.scope,
                    branch: plan.branch.clone(),
                    base_commit: plan.base_commit,
                    base_revision: base_revision.clone(),
                    target: plan.target.clone(),
                    identity: plan.identity,
                    linked: plan.linked,
                };
                self.add_entry(
                    operation,
                    RealGitEntry {
                        path,
                        base_revision,
                        gitdir_fingerprint,
                        gitdir_identity,
                        gitdir_handle,
                        receipt: receipt.clone(),
                    },
                );
                if self.interrupt_after_add.swap(false, Ordering::SeqCst) {
                    return Ok(AddResult::InterruptedAfterSideEffect);
                }
                Ok(AddResult::Applied(receipt))
            }

            fn inspect(
                &self,
                _workspace: &ResolvedWorkspace,
                operation: JournalOperation,
                plan: &WorktreePlan,
                expected_receipt: Option<&$crate::workspace::worktree::CreatedWorktree>,
                cancellation: &CancellationToken,
                budget: $crate::workspace::worktree::ExecutionBudget,
            ) -> Result<RecoveryLookup, ExecutorError> {
                Self::check(cancellation, budget)?;
                let Some(entry) = self.entry(operation) else {
                    return Ok(RecoveryLookup::Absent);
                };
                if !Self::target_present(&entry.path)? {
                    self.remove_entry(operation);
                    return Ok(RecoveryLookup::Absent);
                }
                self.validate_entry(&entry)?;
                if entry.receipt.branch != plan.branch
                    || entry.receipt.scope != plan.scope
                    || entry.receipt.identity != plan.identity
                    || entry.receipt.base_commit != plan.base_commit
                    || entry.receipt.base_revision != plan.base_revision
                    || entry.receipt.target != plan.target
                    || entry.receipt.linked != plan.linked
                    || entry.receipt.scope.repository != plan.repository
                    || expected_receipt.is_some_and(|expected| expected != &entry.receipt)
                {
                    return Err(ExecutorError::IdentityMismatch);
                }
                let listing = self.git(
                    vec![
                        OsString::from("worktree"),
                        OsString::from("list"),
                        OsString::from("--porcelain"),
                        OsString::from("-z"),
                    ],
                    cancellation,
                    budget,
                )?;
                if !listing.status.success() {
                    return Err(ExecutorError::MalformedOutput);
                }
                Ok(RecoveryLookup::Applied(entry.receipt))
            }

            fn compensate(
                &self,
                _workspace: &ResolvedWorkspace,
                operation: JournalOperation,
                _receipt: &$crate::workspace::worktree::CreatedWorktree,
                cancellation: &CancellationToken,
                budget: $crate::workspace::worktree::ExecutionBudget,
            ) -> Result<(), ExecutorError> {
                Self::check(cancellation, budget)?;
                let Some(entry) = self.entry(operation) else {
                    return Ok(());
                };
                self.validate_entry(&entry)?;
                let output = self.git(
                    vec![
                        OsString::from("worktree"),
                        OsString::from("remove"),
                        OsString::from("--force"),
                        entry.path.as_os_str().to_owned(),
                    ],
                    cancellation,
                    budget,
                )?;
                if !output.status.success() {
                    return Err(ExecutorError::CompensationFailed);
                }
                if Self::target_present(&entry.path)? {
                    return Err(ExecutorError::IdentityMismatch);
                }
                self.remove_entry(operation);
                Ok(())
            }

            fn preview(
                &self,
                _workspace: &ResolvedWorkspace,
                receipt: &$crate::workspace::worktree::CreatedWorktree,
                cancellation: &CancellationToken,
                budget: $crate::workspace::worktree::ExecutionBudget,
            ) -> Result<CleanupSnapshot, ExecutorError> {
                Self::check(cancellation, budget)?;
                let Some(entry) = self.entry(JournalOperation {
                    kind: JournalKind::Add,
                    key: receipt.operation_id,
                }) else {
                    return Err(ExecutorError::NotFound);
                };
                self.validate_entry(&entry)?;
                let output = self.git_at(
                    &entry.path,
                    vec![
                        OsString::from("status"),
                        OsString::from("--porcelain=v2"),
                        OsString::from("-z"),
                        OsString::from("--untracked-files=all"),
                    ],
                    cancellation,
                    budget,
                )?;
                if !output.status.success() {
                    return Err(ExecutorError::IdentityMismatch);
                }
                let mut tracked = CleanupState::Clean;
                let mut untracked = CleanupState::Clean;
                for record in output.stdout.split(|byte| *byte == 0) {
                    if record.starts_with(b"? ") {
                        untracked = CleanupState::Dirty;
                    } else if record.starts_with(b"1 ")
                        || record.starts_with(b"2 ")
                        || record.starts_with(b"u ")
                    {
                        tracked = CleanupState::Dirty;
                    }
                }
                let unpushed = self.unpushed_state(&entry, cancellation, budget)?;
                let nested = self.nested_state(&entry.path, cancellation, budget)?;
                let (linked, foreign, main_checkout) =
                    self.worktree_membership(&entry, cancellation, budget)?;
                Ok(CleanupSnapshot {
                    tracked,
                    untracked,
                    unpushed,
                    nested,
                    linked,
                    foreign,
                    main_checkout,
                })
            }

            fn prove_process_zero(
                &self,
                workspace: &ResolvedWorkspace,
                _receipt: &$crate::workspace::worktree::CreatedWorktree,
                cancellation: &CancellationToken,
                budget: $crate::workspace::worktree::ExecutionBudget,
            ) -> Result<ProcessZeroProof, ExecutorError> {
                Self::check(cancellation, budget)?;
                if self.active_children.load(Ordering::Acquire) != 0 {
                    return Err(ExecutorError::ProcessNotZero);
                }
                let fence = workspace
                    .process_fence
                    .clone()
                    .ok_or(ExecutorError::ProcessNotZero)?;
                // The test adapter supplies a fresh observation token. Production
                // wiring must replace this with the authoritative Task3
                // ACTIVE_PROCESS_ZERO proof.
                let zero_observation = self
                    .zero_observation
                    .fetch_add(1, Ordering::SeqCst)
                    .saturating_add(1);
                Ok(ProcessZeroProof {
                    identity: workspace.identity,
                    fence,
                    zero_observation,
                })
            }

            fn remove(
                &self,
                _workspace: &ResolvedWorkspace,
                operation: JournalOperation,
                _receipt: &$crate::workspace::worktree::CreatedWorktree,
                proof: ProcessZeroProof,
                cancellation: &CancellationToken,
                budget: $crate::workspace::worktree::ExecutionBudget,
            ) -> Result<(), ExecutorError> {
                Self::check(cancellation, budget)?;
                if proof.zero_observation == 0 {
                    return Err(ExecutorError::ProcessNotZero);
                }
                let Some(entry) = self.entry(JournalOperation {
                    kind: JournalKind::Add,
                    key: operation.key,
                }) else {
                    return Err(ExecutorError::NotFound);
                };
                self.validate_entry(&entry)?;
                let output = self.git(
                    vec![
                        OsString::from("worktree"),
                        OsString::from("remove"),
                        OsString::from("--force"),
                        entry.path.as_os_str().to_owned(),
                    ],
                    cancellation,
                    budget,
                )?;
                if !output.status.success() {
                    return Err(ExecutorError::CompensationFailed);
                }
                if Self::target_present(&entry.path)? {
                    return Err(ExecutorError::IdentityMismatch);
                }
                self.remove_entry(operation);
                Ok(())
            }
        }

        fn fixture() -> (
            WorktreeService,
            TestWorkspaceAuthorization,
            TestGitWorktreeExecutor,
            TestOperationJournal,
        ) {
            let (authorization, _control) = TestWorkspaceAuthorization::new();
            let executor = TestGitWorktreeExecutor::new();
            let journal = TestOperationJournal::new();
            let service = WorktreeService::for_test(executor.clone(), journal.clone());
            (service, authorization, executor, journal)
        }

        #[test]
        fn branch_collision_attempts_are_bounded_and_idempotent() {
            let (service, authorization, executor, _journal) = fixture();
            executor.set_collision_attempts(3);
            let request = CreateWorktreeRequest::new("Task 42").with_idempotency_key([7; 16]);

            let first = service
                .create_for_test(&authorization, request.clone())
                .expect("collision-safe create");
            let again = service
                .create_for_test(&authorization, request)
                .expect("same request is idempotent");

            assert_eq!(first, again);
            assert_eq!(executor.add_count(), 1);
            assert!(first.branch().starts_with("codex/task-42"));
        }

        #[test]
        fn settled_replay_rejects_a_different_requested_plan_for_the_same_operation_id() {
            let (service, authorization, executor, _journal) = fixture();
            let first = CreateWorktreeRequest::new("first-plan").with_idempotency_key([41; 16]);
            service
                .create_for_test(&authorization, first)
                .expect("initial operation");

            let replay =
                CreateWorktreeRequest::new("different-plan").with_idempotency_key([41; 16]);
            let error = service
                .create_for_test(&authorization, replay)
                .expect_err("same operation id cannot replay a different plan");

            assert!(matches!(error, WorktreeError::WorkspaceChanged));
            assert_eq!(executor.add_count(), 1);
        }

        #[test]
        fn stale_lease_generation_and_action_epoch_are_rejected_before_side_effect() {
            let (service, authorization, executor, _journal) = fixture();
            let control = authorization.control();

            control.bump_action_epoch();
            let error = service
                .create_for_test(&authorization, CreateWorktreeRequest::new("stale-epoch"))
                .expect_err("stale action epoch");
            assert!(matches!(error, WorktreeError::StaleAuthority));
            assert_eq!(executor.add_count(), 0);

            control.restore_action_epoch();
            control.bump_runtime_generation();
            let error = service
                .create_for_test(
                    &authorization,
                    CreateWorktreeRequest::new("stale-generation"),
                )
                .expect_err("stale runtime generation");
            assert!(matches!(error, WorktreeError::StaleAuthority));
            assert_eq!(executor.add_count(), 0);

            control.restore_runtime_generation();
            control.revoke_lease();
            let error = service
                .create_for_test(&authorization, CreateWorktreeRequest::new("revoked-lease"))
                .expect_err("revoked lease");
            assert!(matches!(error, WorktreeError::StaleAuthority));
            assert_eq!(executor.add_count(), 0);
        }

        #[test]
        fn cancellation_and_deadline_are_checked_before_collision_probe() {
            let (service, authorization, executor, _journal) = fixture();
            let cancellation = CancellationToken::new();
            cancellation.cancel();
            let cancelled = service
                .create_for_test(
                    &authorization,
                    CreateWorktreeRequest::new("cancelled").with_cancellation(cancellation),
                )
                .expect_err("cancelled request must not probe");
            assert!(matches!(cancelled, WorktreeError::Cancelled));
            let deadline = service
                .create_for_test(
                    &authorization,
                    CreateWorktreeRequest::new("expired").with_timeout(std::time::Duration::ZERO),
                )
                .expect_err("expired request must not probe");
            assert!(matches!(deadline, WorktreeError::Deadline));
            assert_eq!(executor.active_count(), 0);
        }

        #[test]
        fn root_ancestor_reparse_and_acl_replacement_are_revalidated() {
            for mutate in [
                replace_root as fn(&TestWorkspaceControl),
                replace_ancestor,
                replace_reparse,
                replace_acl,
            ] {
                let (service, authorization, executor, _journal) = fixture();
                mutate(&authorization.control());
                let error = service
                    .create_for_test(&authorization, CreateWorktreeRequest::new("replaced"))
                    .expect_err("replacement must fail closed");
                assert!(matches!(error, WorktreeError::WorkspaceChanged));
                assert_eq!(executor.add_count(), 0);
            }
        }

        fn replace_root(control: &TestWorkspaceControl) {
            control.replace_root_identity();
        }

        fn replace_ancestor(control: &TestWorkspaceControl) {
            control.replace_ancestor_identity();
        }

        fn replace_reparse(control: &TestWorkspaceControl) {
            control.replace_reparse_identity();
        }

        fn replace_acl(control: &TestWorkspaceControl) {
            control.replace_acl_identity();
        }

        #[test]
        fn linked_worktree_gitdir_commondir_backreference_and_repository_identity_are_checked() {
            let (service, authorization, executor, _journal) = fixture();
            executor.replace_linked_identity(true);

            let error = service
                .create_for_test(
                    &authorization,
                    CreateWorktreeRequest::new("wrong-linked-root"),
                )
                .expect_err("linked worktree identity mismatch");
            assert!(matches!(error, WorktreeError::WorkspaceChanged));
            assert_eq!(executor.add_count(), 0);
        }

        #[test]
        fn cancellation_after_add_compensates_or_leaves_recoverable_tombstone() {
            let (service, authorization, executor, journal) = fixture();
            let cancellation = CancellationToken::new();
            executor.cancel_after_add(true);
            executor.fail_compensation(true);

            let error = service
                .create_for_test(
                    &authorization,
                    CreateWorktreeRequest::new("cancel-after-add")
                        .with_cancellation(cancellation.clone()),
                )
                .expect_err("post-side-effect cancellation");
            assert!(matches!(error, WorktreeError::RecoverableOperation));
            assert_eq!(executor.active_count(), 1);
            assert!(journal
                .records()
                .iter()
                .any(|record| record.state() == JournalState::Recoverable));
        }

        #[test]
        fn successful_compensation_revalidates_absence_before_settlement() {
            let (service, authorization, executor, journal) = fixture();
            executor.cancel_after_add(true);
            let error = service
                .create_for_test(&authorization, CreateWorktreeRequest::new("cancel-clean"))
                .expect_err("cancellation should be reported after cleanup");
            assert!(matches!(error, WorktreeError::Cancelled));
            assert_eq!(executor.active_count(), 0);
            assert!(journal
                .records()
                .iter()
                .any(|record| record.state() == JournalState::Compensated));
        }

        #[test]
        fn interrupted_add_is_recovered_idempotently_after_restart() {
            let (service, authorization, executor, journal) = fixture();
            executor.interrupt_after_add(true);
            let request =
                CreateWorktreeRequest::new("restart-recovery").with_idempotency_key([9; 16]);
            let error = service
                .create_for_test(&authorization, request.clone())
                .expect_err("simulated crash after add");
            assert!(matches!(error, WorktreeError::Interrupted));

            executor.interrupt_after_add(false);
            let restarted = WorktreeService::for_test(executor.clone(), journal.clone());
            let report = restarted
                .recover_for_test(&authorization)
                .expect("restart recovery");
            assert_eq!(report.recovered(), 1);
            assert_eq!(executor.active_count(), 1);

            let result = restarted
                .create_for_test(&authorization, request)
                .expect("recovered request is idempotent");
            assert_eq!(executor.add_count(), 1);
            assert_eq!(result.branch(), "codex/restart-recovery");
        }

        #[test]
        fn cleanup_requires_confirmation_and_exact_process_zero() {
            let (service, authorization, executor, journal) = fixture();
            let created = service
                .create_for_test(&authorization, CreateWorktreeRequest::new("cleanup"))
                .expect("create");

            executor.set_dirty(true);
            let error = service
                .remove_for_test(&authorization, &created, CleanupConfirmation::none())
                .expect_err("destructive cleanup requires confirmation");
            assert!(matches!(error, WorktreeError::CleanupRefused));

            executor.set_dirty(false);
            executor.set_process_count(1);
            let error = service
                .remove_for_test(&authorization, &created, CleanupConfirmation::force())
                .expect_err("non-zero process proof");
            assert!(matches!(error, WorktreeError::ProcessNotZero));
            assert_eq!(executor.active_count(), 1);

            executor.set_process_count(0);
            executor.set_process_fence_mismatch(true);
            let error = service
                .remove_for_test(&authorization, &created, CleanupConfirmation::confirmed())
                .expect_err("mismatched process fence");
            assert!(matches!(error, WorktreeError::ProcessNotZero));
            assert_eq!(executor.active_count(), 1);

            executor.set_process_fence_mismatch(false);
            service
                .remove_for_test(&authorization, &created, CleanupConfirmation::confirmed())
                .expect("exact process zero permits removal");
            service
                .remove_for_test(&authorization, &created, CleanupConfirmation::confirmed())
                .expect("settled removal is idempotent");
            assert_eq!(executor.active_count(), 0);
            assert!(journal
                .records()
                .iter()
                .any(|record| record.state() == JournalState::Settled));
        }

        #[test]
        fn cleanup_refuses_ownership_conflicts_and_force_removes_user_state() {
            let (service, authorization, executor, _journal) = fixture();
            let created = service
                .create_for_test(&authorization, CreateWorktreeRequest::new("state-fences"))
                .expect("create");
            let states: [Box<dyn Fn(bool) + '_>; 4] = [
                Box::new(|value| executor.set_nested(value)),
                Box::new(|value| executor.set_linked(value)),
                Box::new(|value| executor.set_foreign(value)),
                Box::new(|value| executor.set_main_checkout(value)),
            ];
            for set_state in states {
                set_state(true);
                let error = service
                    .remove_for_test(&authorization, &created, CleanupConfirmation::force())
                    .expect_err("user or foreign state must refuse removal");
                assert!(matches!(error, WorktreeError::CleanupRefused));
                set_state(false);
            }

            executor.set_dirty(true);
            let confirmed = service
                .remove_for_test(&authorization, &created, CleanupConfirmation::confirmed())
                .expect_err("ordinary confirmation must refuse user dirt");
            assert!(matches!(confirmed, WorktreeError::CleanupRefused));
            service
                .remove_for_test(&authorization, &created, CleanupConfirmation::force())
                .expect("force confirmation removes exact user-dirty target");
        }

        #[test]
        fn already_absent_cleanup_revalidates_before_terminal_settlement() {
            let (authorization, control) = TestWorkspaceAuthorization::new();
            let executor = TestGitWorktreeExecutor::new();
            let journal = TestOperationJournal::new();
            let service = WorktreeService::for_test(executor.clone(), journal.clone());
            let created = service
                .create_for_test(&authorization, CreateWorktreeRequest::new("absent-fence"))
                .expect("create");

            let add_operation = JournalOperation {
                kind: JournalKind::Add,
                key: created.operation_id,
            };
            executor.forget(add_operation);
            executor.invalidate_after_preview(control);

            let error = service
                .remove_for_test(&authorization, &created, CleanupConfirmation::confirmed())
                .expect_err("an authority change during an absent preview must not settle");
            assert!(matches!(error, WorktreeError::StaleAuthority));
            assert_eq!(journal.reservation_count(), 0);
            assert!(journal
                .records()
                .iter()
                .any(|record| record.state() == JournalState::Recoverable));
        }

        #[test]
        fn cancellation_after_remove_leaves_a_visible_recoverable_record() {
            let (service, authorization, executor, journal) = fixture();
            let created = service
                .create_for_test(&authorization, CreateWorktreeRequest::new("remove-cancel"))
                .expect("create");
            executor.cancel_after_remove(true);

            let error = service
                .remove_for_test(&authorization, &created, CleanupConfirmation::confirmed())
                .expect_err("cancellation after deletion must remain recoverable");
            assert!(matches!(error, WorktreeError::RecoverableOperation));
            assert_eq!(executor.active_count(), 0);
            assert!(journal
                .records()
                .iter()
                .any(|record| record.state() == JournalState::Recoverable));
        }

        #[test]
        fn two_service_instances_reserve_before_collision_probe() {
            let (authorization, _control) = TestWorkspaceAuthorization::new();
            let executor = TestGitWorktreeExecutor::new();
            let journal = TestOperationJournal::new();
            let first_service = WorktreeService::for_test(executor.clone(), journal.clone());
            let second_service = WorktreeService::for_test(executor.clone(), journal.clone());
            let first_authorization = authorization.clone();
            let second_authorization = authorization.clone();

            let first = std::thread::spawn(move || {
                first_service.create_for_test(
                    &first_authorization,
                    CreateWorktreeRequest::new("parallel").with_idempotency_key([61; 16]),
                )
            });
            let second = std::thread::spawn(move || {
                second_service.create_for_test(
                    &second_authorization,
                    CreateWorktreeRequest::new("parallel").with_idempotency_key([62; 16]),
                )
            });
            let first = first
                .join()
                .expect("first operation thread")
                .expect("first create");
            let second = second
                .join()
                .expect("second operation thread")
                .expect("second create");

            assert_ne!(first.branch(), second.branch());
            assert_eq!(executor.add_count(), 2);
        }

        #[test]
        fn independent_sqlite_journal_instances_reserve_with_durable_cas() {
            let directory = tempfile::tempdir().expect("journal temp directory");
            let path = directory.path().join("concurrent-journal.sqlite");
            let first_journal = SqliteTestJournal::open(&path).expect("first journal");
            let second_journal = SqliteTestJournal::open(&path).expect("second journal");
            let executor = TestGitWorktreeExecutor::new();
            let (authorization, _control) = TestWorkspaceAuthorization::new();
            let first_service = WorktreeService::from_process_owned(
                std::sync::Arc::new(executor.clone()),
                std::sync::Arc::new(first_journal),
            );
            let second_service = WorktreeService::from_process_owned(
                std::sync::Arc::new(executor.clone()),
                std::sync::Arc::new(second_journal),
            );
            let first_authorization = authorization.clone();
            let second_authorization = authorization.clone();
            let first = std::thread::spawn(move || {
                first_service.create_for_test(
                    &first_authorization,
                    CreateWorktreeRequest::new("sqlite-race-a"),
                )
            });
            let second = std::thread::spawn(move || {
                second_service.create_for_test(
                    &second_authorization,
                    CreateWorktreeRequest::new("sqlite-race-b"),
                )
            });
            let first = first.join().expect("first sqlite operation");
            let second = second.join().expect("second sqlite operation");
            assert!(
                first.is_ok(),
                "first sqlite operation failed: {}",
                first
                    .as_ref()
                    .err()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "ok".to_string())
            );
            assert!(
                second.is_ok(),
                "second sqlite operation failed: {}",
                second
                    .as_ref()
                    .err()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "ok".to_string())
            );
            assert_eq!(executor.add_count(), 2);
        }

        #[test]
        fn same_key_loser_cannot_release_or_recover_paused_winner() {
            let (authorization, _control) = TestWorkspaceAuthorization::new();
            let executor = TestGitWorktreeExecutor::new();
            let journal = TestOperationJournal::new();
            let winner_service = WorktreeService::for_test(executor.clone(), journal.clone());
            let loser_service = WorktreeService::for_test(executor.clone(), journal.clone());
            let (entered, release) = executor.pause_before_add();
            let winner_authorization = authorization.clone();
            let winner = std::thread::spawn(move || {
                winner_service.create_for_test(
                    &winner_authorization,
                    CreateWorktreeRequest::new("same-key").with_idempotency_key([63; 16]),
                )
            });
            entered.wait();

            let loser = loser_service
                .create_for_test(
                    &authorization,
                    CreateWorktreeRequest::new("same-key").with_idempotency_key([63; 16]),
                )
                .expect_err("duplicate caller must not recover the winner");
            assert!(matches!(
                loser,
                WorktreeError::OperationInFlight | WorktreeError::TargetCollision
            ));
            assert_eq!(executor.active_count(), 0);
            assert!(journal.reservation_count() > 0);

            release.wait();
            winner
                .join()
                .expect("winner thread")
                .expect("winner completes");
            assert_eq!(executor.add_count(), 1);
        }

        #[test]
        fn recovery_ignores_transient_client_scope_and_does_not_tombstone_it() {
            let (service, authorization, executor, journal) = fixture();
            let request =
                CreateWorktreeRequest::new("stable-recovery").with_idempotency_key([64; 16]);
            executor.interrupt_after_add(true);
            let error = service
                .create_for_test(&authorization, request)
                .expect_err("interrupted create leaves recovery intent");
            assert!(matches!(error, WorktreeError::Interrupted));
            executor.interrupt_after_add(false);
            authorization.control().bump_transient_scope();

            let restarted = WorktreeService::for_test(executor.clone(), journal.clone());
            let report = restarted
                .recover_for_test(&authorization)
                .expect("recovery should use stable authority");
            assert_eq!(report.tombstones(), 0);
            assert_eq!(report.recovered(), 1);
            assert!(journal
                .records()
                .iter()
                .all(|record| record.state() != JournalState::Recoverable));
        }

        #[test]
        fn recovery_never_tombstones_a_different_stable_workspace_scope() {
            let executor = TestGitWorktreeExecutor::new();
            let journal = TestOperationJournal::new();
            let first_service = WorktreeService::for_test(executor.clone(), journal.clone());
            let second_service = WorktreeService::for_test(executor.clone(), journal.clone());
            let (first_authorization, _first_control) = TestWorkspaceAuthorization::new();
            let (second_authorization, _second_control) = TestWorkspaceAuthorization::new();
            executor.interrupt_after_add(true);
            let interrupted = first_service
                .create_for_test(
                    &first_authorization,
                    CreateWorktreeRequest::new("other-scope"),
                )
                .expect_err("first scope leaves an intent");
            assert!(matches!(interrupted, WorktreeError::Interrupted));
            executor.interrupt_after_add(false);
            let report = second_service
                .recover_for_test(&second_authorization)
                .expect("other scope recovery");
            assert_eq!(report.recovered(), 0);
            assert_eq!(report.tombstones(), 0);
            assert_eq!(executor.active_count(), 1);
            assert!(journal
                .records()
                .iter()
                .any(|record| record.state() == JournalState::Intent));
        }

        #[test]
        fn malformed_and_oversize_porcelain_fail_closed_with_redacted_errors() {
            let valid = parse_worktree_porcelain(
                b"worktree opaque-root\0HEAD abc-def\0branch refs/heads/codex/task\0\0",
            )
            .expect("valid bounded porcelain");
            assert_eq!(valid.len(), 1);

            let malformed = parse_worktree_porcelain(b"worktree\0branch").expect_err("malformed");
            assert_eq!(format!("{malformed}"), "git worktree output is malformed");
            assert!(!format!("{malformed:?}").contains("worktree"));

            let oversized = vec![b'x'; MAX_PORCELAIN_BYTES + 1];
            let error = parse_worktree_porcelain(&oversized).expect_err("oversize");
            assert_eq!(format!("{error}"), "git worktree output exceeds the limit");
        }

        #[test]
        fn registry_and_history_stay_bounded() {
            let (service, authorization, _executor, journal) = fixture();
            let mut refused = false;
            for index in 0..(MAX_JOURNAL_OPERATIONS + 8) {
                let request = CreateWorktreeRequest::new(format!("bounded-{index}"))
                    .with_idempotency_key((index as u128).to_le_bytes());
                if service.create_for_test(&authorization, request).is_err() {
                    refused = true;
                    break;
                }
            }
            assert!(refused, "bounded registry must refuse growth");
            assert!(journal.records().len() <= MAX_JOURNAL_OPERATIONS);
        }

        #[test]
        fn sqlite_journal_reopen_preserves_intent_and_cas() {
            let directory = tempfile::tempdir().expect("journal temp directory");
            let path = directory.path().join("worktree-journal.sqlite");
            let (authorization, _control) = TestWorkspaceAuthorization::new();
            let executor = TestGitWorktreeExecutor::new();
            let journal = SqliteTestJournal::open(&path).expect("open sqlite journal");
            let service = WorktreeService::from_process_owned(
                std::sync::Arc::new(executor.clone()),
                std::sync::Arc::new(journal.clone()),
            );
            executor.interrupt_after_add(true);
            let request =
                CreateWorktreeRequest::new("sqlite-reopen").with_idempotency_key([90; 16]);
            let error = service
                .create_for_test(&authorization, request)
                .expect_err("simulated crash leaves durable intent");
            assert!(matches!(error, WorktreeError::Interrupted));
            drop(service);
            drop(journal);

            let reopened = SqliteTestJournal::open(&path).expect("reopen sqlite journal");
            let operation = JournalOperation {
                kind: JournalKind::Add,
                key: OperationKey([90; 16]),
            };
            let cancellation = CancellationToken::new();
            let journal_context = JournalContext::new(
                &cancellation,
                ExecutionBudget::from_timeout(std::time::Duration::from_secs(5)),
            );
            let record = reopened
                .get(operation, journal_context)
                .expect("read durable intent")
                .expect("intent exists after reopen");
            assert_eq!(record.state(), JournalState::Intent);
            assert!(matches!(
                reopened.insert_intent(record.clone(), journal_context),
                Err($crate::workspace::worktree::JournalError::Duplicate)
            ));
            assert_eq!(
                reopened
                    .get(operation, journal_context)
                    .expect("read after duplicate")
                    .expect("intent remains")
                    .state(),
                JournalState::Intent
            );
            assert!(matches!(
                reopened.update_owned_cas(
                    operation,
                    record.owner(),
                    99,
                    JournalState::Intent,
                    JournalState::Settled,
                    None,
                    journal_context,
                ),
                Err($crate::workspace::worktree::JournalError::CasMismatch)
            ));
            reopened
                .release(
                    &record.scope,
                    &record.plan.branch,
                    operation,
                    record.owner(),
                    journal_context,
                )
                .expect("simulate a recoverable reservation gap");

            executor.interrupt_after_add(false);
            let restarted = WorktreeService::from_process_owned(
                std::sync::Arc::new(executor.clone()),
                std::sync::Arc::new(reopened.clone()),
            );
            let report = restarted
                .recover_for_test(&authorization)
                .expect("recover reopened intent");
            assert_eq!(report.recovered(), 1);
            assert_eq!(
                reopened
                    .get(operation, journal_context)
                    .expect("read settled record")
                    .expect("record remains")
                    .state(),
                JournalState::Settled
            );
        }

        #[test]
        fn sqlite_journal_rejects_a_path_that_is_not_the_retained_store_handle() {
            let directory = tempfile::tempdir().expect("journal temp directory");
            let admitted = directory.path().join("admitted.sqlite");
            let substituted = directory.path().join("substituted.sqlite");
            std::fs::File::create(&admitted).expect("admitted store");
            std::fs::File::create(&substituted).expect("substituted store");
            let handle = Arc::new(std::fs::File::open(&admitted).expect("retained handle"));
            let store = WorktreeJournalStore::from_validated(substituted, handle, [7; 32])
                .expect("host seam accepts the typed store");
            assert!(matches!(
                SqliteWorktreeJournal::from_store(Arc::new(store)),
                Err(JournalError::InvalidStore)
            ));
        }

        #[test]
        fn production_executor_is_fail_closed_and_errors_do_not_leak_details() {
            let error = WorktreeService::new().expect_err("production wiring is not accepted yet");
            assert!(matches!(
                error,
                WorktreeError::Hold(WorktreeHold::Task3AuthorityUnavailable)
            ));
            assert_eq!(error.to_string(), "Git worktree authority is unavailable");
            assert!(!format!("{error:?}").contains("git"));
            assert!(!format!("{error:?}").contains("stderr"));
            assert!(!format!("{error:?}").contains("C:"));
        }

        #[test]
        fn force_confirmation_allows_user_dirty_state_but_not_ownership_conflicts() {
            let (service, authorization, executor, _journal) = fixture();
            let created = service
                .create_for_test(
                    &authorization,
                    CreateWorktreeRequest::new("force-user-state"),
                )
                .expect("create");

            executor.set_dirty(true);
            service
                .remove_for_test(&authorization, &created, CleanupConfirmation::force())
                .expect("force confirmation removes service-owned user dirty state");

            let (service, authorization, executor, _journal) = fixture();
            let created = service
                .create_for_test(&authorization, CreateWorktreeRequest::new("force-nested"))
                .expect("create");
            executor.set_nested(true);
            let error = service
                .remove_for_test(&authorization, &created, CleanupConfirmation::force())
                .expect_err("force must not cross a nested repository ownership boundary");
            assert!(matches!(error, WorktreeError::CleanupRefused));
        }

        #[test]
        fn real_git_adapter_exercises_create_remove_and_reconnect_contract() {
            let repository = tempfile::tempdir().expect("temporary Git repository");
            let executor = RealGitWorktreeExecutor::new(repository.path());
            for args in [
                vec![OsString::from("init"), OsString::from("--quiet")],
                vec![
                    OsString::from("config"),
                    OsString::from("user.email"),
                    OsString::from("test@example.invalid"),
                ],
                vec![
                    OsString::from("config"),
                    OsString::from("user.name"),
                    OsString::from("Worktree Contract Test"),
                ],
            ] {
                assert!(executor
                    .setup_git(args)
                    .expect("git setup command")
                    .status
                    .success());
            }
            std::fs::write(repository.path().join("README.md"), "fixture\n").expect("fixture file");
            for args in [
                vec![
                    OsString::from("add"),
                    OsString::from("--"),
                    OsString::from("README.md"),
                ],
                vec![
                    OsString::from("commit"),
                    OsString::from("--quiet"),
                    OsString::from("-m"),
                    OsString::from("fixture"),
                ],
            ] {
                assert!(executor
                    .setup_git(args)
                    .expect("git commit command")
                    .status
                    .success());
            }

            let (authorization, _control) = TestWorkspaceAuthorization::new();
            let directory = tempfile::tempdir().expect("journal directory");
            let journal_path = directory.path().join("worktree-journal.sqlite");
            let journal = SqliteTestJournal::open(&journal_path).expect("journal");
            let service = WorktreeService::from_process_owned(
                Arc::new(executor.clone()),
                Arc::new(journal.clone()),
            );
            let request = CreateWorktreeRequest::new("real Git contract")
                .with_branch("codex/real-git-contract")
                .with_idempotency_key([121; 16])
                .with_test_target(executor.target_for(JournalOperation {
                    kind: JournalKind::Add,
                    key: OperationKey([121; 16]),
                }));
            let receipt = service
                .create_for_test(&authorization, request.clone())
                .expect("real Git create through executor contract");
            let add_operation = JournalOperation {
                kind: JournalKind::Add,
                key: OperationKey([121; 16]),
            };
            let linked = executor.linked_path(add_operation);
            assert!(linked.exists(), "Git worktree add must create the target");
            assert_eq!(receipt.target.path, linked);
            assert_eq!(receipt.target.approved_root, repository.path());
            assert!(!receipt.base_revision.is_empty());
            assert_eq!(
                service
                    .create_for_test(&authorization, request.clone())
                    .expect("same-key replay"),
                receipt
            );

            drop(service);
            drop(journal);
            let reopened = SqliteTestJournal::open(&journal_path).expect("reopen journal");
            let restarted =
                WorktreeService::from_process_owned(Arc::new(executor.clone()), Arc::new(reopened));
            assert_eq!(
                restarted
                    .create_for_test(&authorization, request)
                    .expect("reconnected settled replay"),
                receipt
            );
            restarted
                .remove_for_test(&authorization, &receipt, CleanupConfirmation::force())
                .expect("real Git remove through executor contract");
            assert!(
                !linked.exists(),
                "removed Git worktree must leave no target"
            );
            let listing = executor
                .setup_git(vec![
                    OsString::from("worktree"),
                    OsString::from("list"),
                    OsString::from("--porcelain"),
                    OsString::from("-z"),
                ])
                .expect("Git worktree listing");
            assert!(listing.status.success());
            let linked_text = linked.to_string_lossy();
            assert!(!String::from_utf8_lossy(&listing.stdout).contains(linked_text.as_ref()));
            assert_eq!(executor.active_child_count(), 0);
        }

        #[test]
        fn real_git_cleanup_refuses_tracked_untracked_unpushed_and_nested_state() {
            let repository = tempfile::tempdir().expect("temporary Git repository");
            let executor = RealGitWorktreeExecutor::new(repository.path());
            for args in [
                vec![OsString::from("init"), OsString::from("--quiet")],
                vec![
                    OsString::from("config"),
                    OsString::from("user.email"),
                    OsString::from("test@example.invalid"),
                ],
                vec![
                    OsString::from("config"),
                    OsString::from("user.name"),
                    OsString::from("Worktree Cleanup Test"),
                ],
            ] {
                assert!(executor
                    .setup_git(args)
                    .expect("git setup")
                    .status
                    .success());
            }
            std::fs::write(repository.path().join("README.md"), "fixture\n").expect("fixture file");
            for args in [
                vec![
                    OsString::from("add"),
                    OsString::from("--"),
                    OsString::from("README.md"),
                ],
                vec![
                    OsString::from("commit"),
                    OsString::from("--quiet"),
                    OsString::from("-m"),
                    OsString::from("fixture"),
                ],
            ] {
                assert!(executor
                    .setup_git(args)
                    .expect("git commit")
                    .status
                    .success());
            }

            let (authorization, _control) = TestWorkspaceAuthorization::new();
            let directory = tempfile::tempdir().expect("journal directory");
            let journal =
                SqliteTestJournal::open(directory.path().join("journal.sqlite").as_path())
                    .expect("journal");
            let service = WorktreeService::from_process_owned(
                Arc::new(executor.clone()),
                Arc::new(journal.clone()),
            );
            let reconnect = || {
                WorktreeService::from_process_owned(
                    Arc::new(executor.clone()),
                    Arc::new(journal.clone()),
                )
            };
            let request = CreateWorktreeRequest::new("real cleanup states")
                .with_branch("codex/real-cleanup-states")
                .with_idempotency_key([124; 16])
                .with_test_target(executor.target_for(JournalOperation {
                    kind: JournalKind::Add,
                    key: OperationKey([124; 16]),
                }));
            let receipt = service
                .create_for_test(&authorization, request)
                .expect("create worktree");
            let entry = executor
                .entry(JournalOperation {
                    kind: JournalKind::Add,
                    key: receipt.operation_id,
                })
                .expect("retained real entry");

            std::fs::write(entry.path.join("README.md"), "changed\n").expect("dirty file");
            let tracked_result = reconnect().remove_for_test(
                &authorization,
                &receipt,
                CleanupConfirmation::confirmed(),
            );
            assert!(
                matches!(&tracked_result, Err(WorktreeError::CleanupRefused)),
                "tracked cleanup must refuse"
            );
            assert!(executor
                .git_at(
                    &entry.path,
                    vec![
                        OsString::from("reset"),
                        OsString::from("--hard"),
                        OsString::from("HEAD")
                    ],
                    &CancellationToken::new(),
                    ExecutionBudget::from_timeout(Duration::from_secs(10)),
                )
                .expect("reset tracked state")
                .status
                .success());

            std::fs::write(entry.path.join("untracked.txt"), "untracked\n")
                .expect("untracked file");
            let untracked_result = reconnect().remove_for_test(
                &authorization,
                &receipt,
                CleanupConfirmation::confirmed(),
            );
            assert!(
                matches!(&untracked_result, Err(WorktreeError::CleanupRefused)),
                "untracked cleanup must refuse"
            );
            std::fs::remove_file(entry.path.join("untracked.txt")).expect("remove untracked");

            std::fs::write(entry.path.join("unpushed.txt"), "unpushed\n").expect("unpushed file");
            assert!(executor
                .git_at(
                    &entry.path,
                    vec![
                        OsString::from("add"),
                        OsString::from("--"),
                        OsString::from("unpushed.txt")
                    ],
                    &CancellationToken::new(),
                    ExecutionBudget::from_timeout(Duration::from_secs(10)),
                )
                .expect("stage unpushed commit")
                .status
                .success());
            assert!(executor
                .git_at(
                    &entry.path,
                    vec![
                        OsString::from("commit"),
                        OsString::from("--quiet"),
                        OsString::from("-m"),
                        OsString::from("unpushed"),
                    ],
                    &CancellationToken::new(),
                    ExecutionBudget::from_timeout(Duration::from_secs(10)),
                )
                .expect("create unpushed commit")
                .status
                .success());
            let unpushed_result = reconnect().remove_for_test(
                &authorization,
                &receipt,
                CleanupConfirmation::confirmed(),
            );
            assert!(
                matches!(&unpushed_result, Err(WorktreeError::CleanupRefused)),
                "unpushed cleanup must refuse"
            );
            assert!(executor
                .git_at(
                    &entry.path,
                    vec![
                        OsString::from("reset"),
                        OsString::from("--hard"),
                        OsString::from("HEAD~1")
                    ],
                    &CancellationToken::new(),
                    ExecutionBudget::from_timeout(Duration::from_secs(10)),
                )
                .expect("reset unpushed state")
                .status
                .success());

            let nested = entry.path.join("nested-repository");
            std::fs::create_dir(&nested).expect("nested directory");
            assert!(executor
                .git_at(
                    &nested,
                    vec![OsString::from("init"), OsString::from("--quiet")],
                    &CancellationToken::new(),
                    ExecutionBudget::from_timeout(Duration::from_secs(10)),
                )
                .expect("nested repository")
                .status
                .success());
            let nested_result =
                reconnect().remove_for_test(&authorization, &receipt, CleanupConfirmation::force());
            assert!(
                matches!(&nested_result, Err(WorktreeError::CleanupRefused)),
                "nested repository cleanup must refuse"
            );
            std::fs::remove_dir_all(&nested).expect("remove nested repository");

            reconnect()
                .remove_for_test(&authorization, &receipt, CleanupConfirmation::force())
                .expect("clean exact worktree removes");
            assert!(!entry.path.exists());
            assert_eq!(executor.active_child_count(), 0);
        }

        #[test]
        fn real_git_adapter_scopes_recovery_and_rejects_swapped_target_identity() {
            let repository = tempfile::tempdir().expect("temporary Git repository");
            let executor = RealGitWorktreeExecutor::new(repository.path());
            for args in [
                vec![OsString::from("init"), OsString::from("--quiet")],
                vec![
                    OsString::from("config"),
                    OsString::from("user.email"),
                    OsString::from("test@example.invalid"),
                ],
                vec![
                    OsString::from("config"),
                    OsString::from("user.name"),
                    OsString::from("Worktree Recovery Test"),
                ],
            ] {
                assert!(executor
                    .setup_git(args)
                    .expect("git setup command")
                    .status
                    .success());
            }
            std::fs::write(repository.path().join("README.md"), "fixture\n").expect("fixture file");
            for args in [
                vec![
                    OsString::from("add"),
                    OsString::from("--"),
                    OsString::from("README.md"),
                ],
                vec![
                    OsString::from("commit"),
                    OsString::from("--quiet"),
                    OsString::from("-m"),
                    OsString::from("fixture"),
                ],
            ] {
                assert!(executor
                    .setup_git(args)
                    .expect("git commit command")
                    .status
                    .success());
            }

            let (authorization, _control) = TestWorkspaceAuthorization::new();
            let (other_authorization, _other_control) = TestWorkspaceAuthorization::new();
            let directory = tempfile::tempdir().expect("journal directory");
            let journal_path = directory.path().join("worktree-journal.sqlite");
            let journal = SqliteTestJournal::open(&journal_path).expect("journal");
            let service = WorktreeService::from_process_owned(
                Arc::new(executor.clone()),
                Arc::new(journal.clone()),
            );
            let request = CreateWorktreeRequest::new("real Git recovery")
                .with_branch("codex/real-git-recovery")
                .with_idempotency_key([122; 16])
                .with_test_target(executor.target_for(JournalOperation {
                    kind: JournalKind::Add,
                    key: OperationKey([122; 16]),
                }));
            executor.interrupt_after_add(true);
            assert!(matches!(
                service.create_for_test(&authorization, request),
                Err(WorktreeError::Interrupted)
            ));
            drop(service);

            let add_operation = JournalOperation {
                kind: JournalKind::Add,
                key: OperationKey([122; 16]),
            };
            let linked = executor.linked_path(add_operation);
            assert!(linked.exists(), "interrupted Git add leaves a real target");
            let restarted = WorktreeService::from_process_owned(
                Arc::new(executor.clone()),
                Arc::new(SqliteTestJournal::open(&journal_path).expect("reopen journal")),
            );
            let other_report = restarted
                .recover_for_test(&other_authorization)
                .expect("other scope recovery");
            assert_eq!(other_report.recovered(), 0);
            assert_eq!(other_report.tombstones(), 0);
            assert!(
                linked.exists(),
                "other scope must not inspect or tombstone it"
            );

            // The retained `.git` descriptor intentionally prevents a Windows
            // directory rename.  Swap its content instead; this exercises the
            // same identity/reparse race without dropping the required live
            // handle merely to make the fixture destructive.
            std::fs::write(linked.join(".git"), "gitdir: swapped\n")
                .expect("swap the approved gitdir content");
            let report = restarted
                .recover_for_test(&authorization)
                .expect("swapped target recovery");
            assert_eq!(report.recovered(), 0);
            assert_eq!(report.tombstones(), 1);
            assert!(
                linked.join(".git").exists(),
                "swapped identity must remain visible for recovery"
            );
            assert!(journal
                .records(
                    MAX_JOURNAL_OPERATIONS,
                    JournalContext::new(
                        &CancellationToken::new(),
                        ExecutionBudget::from_timeout(std::time::Duration::from_secs(5)),
                    ),
                )
                .expect("read recovery record")
                .iter()
                .any(|record| record.state() == JournalState::Recoverable));

            std::fs::remove_dir_all(&linked).expect("cleanup swapped target");
            assert_eq!(executor.active_child_count(), 0);
        }

        #[test]
        fn recovery_does_not_reopen_a_settled_record_after_reservation_release() {
            let (service, authorization, executor, journal) = fixture();
            let request = CreateWorktreeRequest::new("settlement-fence")
                .with_branch("codex/settlement-fence")
                .with_idempotency_key([123; 16]);
            executor.interrupt_after_add(true);
            assert!(matches!(
                service.create_for_test(&authorization, request),
                Err(WorktreeError::Interrupted)
            ));

            // A live-authority change after the durable settlement must not
            // make the already released reservation look recoverable. The
            // hook models the boundary between the atomic settle/release and
            // any caller observing the result.
            journal.invalidate_after_settlement(authorization.control());
            let restarted = WorktreeService::for_test(executor, journal.clone());
            let report = restarted
                .recover_for_test(&authorization)
                .expect("settled recovery remains authoritative");
            assert_eq!(report.recovered(), 1);
            assert!(journal
                .records()
                .iter()
                .any(|record| record.state() == JournalState::Settled));
            assert_eq!(journal.reservation_count(), 0);
        }

        #[test]
        fn bounded_git_runner_cancellation_owns_and_joins_every_reader() {
            let repository = tempfile::tempdir().expect("temporary Git repository");
            let executor = RealGitWorktreeExecutor::new(repository.path());
            let cancellation = CancellationToken::new();
            cancellation.cancel();
            let error = executor
                .git(
                    vec![OsString::from("version")],
                    &cancellation,
                    ExecutionBudget::from_timeout(Duration::from_secs(5)),
                )
                .expect_err("cancelled child must not spawn");
            assert!(matches!(error, ExecutorError::Cancelled));
            assert_eq!(executor.active_child_count(), 0);
        }

        #[test]
        fn bounded_git_runner_cancellation_after_spawn_reaps_readers() {
            let repository = tempfile::tempdir().expect("temporary Git repository");
            let executor = RealGitWorktreeExecutor::new(repository.path());
            let cancellation = CancellationToken::new();
            let canceller = cancellation.clone();
            let cancel_thread = thread::spawn(move || {
                thread::sleep(Duration::from_millis(100));
                canceller.cancel();
            });
            let result = executor.git(
                vec![
                    OsString::from("daemon"),
                    OsString::from("--reuseaddr"),
                    OsString::from("--export-all"),
                    OsString::from(format!(
                        "--base-path={}",
                        repository.path().to_string_lossy()
                    )),
                    OsString::from("--listen=127.0.0.1"),
                    OsString::from("--port=9418"),
                ],
                &cancellation,
                ExecutionBudget::from_timeout(Duration::from_secs(2)),
            );
            cancel_thread.join().expect("cancellation thread");
            match result {
                Err(ExecutorError::Cancelled) => {}
                Err(error) => panic!("unexpected bounded cancellation result: {error}"),
                Ok(output) => panic!(
                    "daemon exited before cancellation with status {:?}",
                    output.status.code()
                ),
            }
            assert_eq!(executor.active_child_count(), 0);
        }
    };
}

// Keep at least one real integration test in this Cargo target.  The
// process-owned Git executor is intentionally still unavailable in production;
// this small adapter owns all command construction so the test cannot bypass
// the prompt/argument-array contract accidentally.
#[test]
fn real_git_repository_fixture_uses_prompt_fenced_argument_arrays() {
    struct GitFixtureAdapter<'a> {
        repository: &'a std::path::Path,
    }

    impl GitFixtureAdapter<'_> {
        fn run(&self, args: Vec<std::ffi::OsString>) -> std::process::Output {
            let mut child = std::process::Command::new("git")
                .args(args)
                .env("GIT_TERMINAL_PROMPT", "0")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .current_dir(self.repository)
                .spawn()
                .expect("git executable");
            let stdout = child.stdout.take().expect("git stdout");
            let stderr = child.stderr.take().expect("git stderr");
            let read = |mut reader: Box<dyn std::io::Read + Send>| {
                std::thread::spawn(move || {
                    let mut bytes = Vec::new();
                    let mut buffer = [0u8; 8192];
                    let mut overflow = false;
                    loop {
                        let count = match reader.read(&mut buffer) {
                            Ok(0) => break,
                            Err(error) => panic!("bounded Git fixture reader failed: {error}"),
                            Ok(count) => count,
                        };
                        let remaining = 256 * 1024usize - bytes.len();
                        overflow |= count > remaining;
                        bytes.extend_from_slice(&buffer[..count.min(remaining)]);
                    }
                    (bytes, overflow)
                })
            };
            let stdout_thread = read(Box::new(stdout));
            let stderr_thread = read(Box::new(stderr));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let status = loop {
                if let Some(status) = child.try_wait().expect("git wait") {
                    break status;
                }
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("bounded Git fixture exceeded its deadline");
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            };
            let (stdout, stdout_overflow) = stdout_thread.join().expect("stdout reader join");
            let (stderr, stderr_overflow) = stderr_thread.join().expect("stderr reader join");
            assert!(
                !stdout_overflow && !stderr_overflow,
                "Git fixture output exceeded cap"
            );
            std::process::Output {
                status,
                stdout,
                stderr,
            }
        }

        fn run_literals(&self, args: &[&str]) -> std::process::Output {
            self.run(
                args.iter()
                    .map(|arg| std::ffi::OsString::from(arg))
                    .collect(),
            )
        }

        fn add_detached(&self, target: &std::path::Path) -> std::process::Output {
            self.run(vec![
                std::ffi::OsString::from("worktree"),
                std::ffi::OsString::from("add"),
                std::ffi::OsString::from("--quiet"),
                std::ffi::OsString::from("--detach"),
                target.as_os_str().to_owned(),
                std::ffi::OsString::from("HEAD"),
            ])
        }

        fn remove(&self, target: &std::path::Path) -> std::process::Output {
            self.run(vec![
                std::ffi::OsString::from("worktree"),
                std::ffi::OsString::from("remove"),
                std::ffi::OsString::from("--force"),
                target.as_os_str().to_owned(),
            ])
        }
    }

    let repository = tempfile::tempdir().expect("temporary Git repository");
    let adapter = GitFixtureAdapter {
        repository: repository.path(),
    };

    assert!(adapter
        .run_literals(&["init", "--quiet", "--initial-branch=main"])
        .status
        .success());
    assert!(adapter
        .run_literals(&["config", "user.email", "test@example.invalid"])
        .status
        .success());
    assert!(adapter
        .run_literals(&["config", "user.name", "Worktree Test"])
        .status
        .success());
    std::fs::write(repository.path().join("README.md"), "fixture\n").expect("fixture file");
    assert!(adapter
        .run_literals(&["add", "--", "README.md"])
        .status
        .success());
    assert!(adapter
        .run_literals(&["commit", "--quiet", "-m", "fixture"])
        .status
        .success());

    let linked = repository.path().join("linked-worktree");
    assert!(adapter.add_detached(&linked).status.success());

    let porcelain = adapter.run_literals(&["worktree", "list", "--porcelain", "-z"]);
    assert!(porcelain.status.success());
    assert!(porcelain
        .stdout
        .split(|byte| *byte == 0)
        .any(|field| field.starts_with(b"worktree ")));
    assert!(porcelain
        .stdout
        .split(|byte| *byte == 0)
        .any(|field| field.starts_with(b"HEAD ")));

    assert!(adapter.remove(&linked).status.success());
    assert!(!linked.exists(), "linked worktree must not be orphaned");
}
