use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{error, info};

use crate::errors::TrainError;

/// A wrapper around the git command line tool.
#[derive(Clone)]
pub struct GitRepository {
    repo_path: PathBuf,
}

impl GitRepository {
    /// Create a new `GitRepository` instance for the repository at the given path.
    pub fn new(repo_path: &Path) -> Result<Self> {
        let repo_path = repo_path.to_path_buf();
        if !repo_path.join(".git").exists() {
            return Err(TrainError::GitError {
                message: "Not a git repository".to_string(),
            }
            .into());
        }
        Ok(Self { repo_path })
    }

    /// Find the git repository root and create a new `GitRepository` instance.
    pub fn new_from_current_dir() -> Result<Self> {
        let output = run_cmd(&["rev-parse", "--show-toplevel"], ".")?;
        let repo_path = PathBuf::from(output.trim());
        Self::new(&repo_path)
    }

    /// Run a git command and return its output.
    pub fn run(&self, args: &[&str]) -> Result<String> {
        run_cmd(args, &self.repo_path)
    }

    pub fn get_current_branch(&self) -> Result<String> {
        self.run(&["branch", "--show-current"])
    }

    pub fn get_current_commit_hash(&self) -> Result<String> {
        let output = self.run(&["rev-parse", "HEAD"])?;
        Ok(output.trim().to_string())
    }

    pub fn get_commit_hash_for_branch(&self, branch_name: &str) -> Result<String> {
        let output = self.run(&["rev-parse", branch_name])?;
        Ok(output.trim().to_string())
    }

    pub fn get_commit_message_for_branch(&self, branch_name: &str) -> Result<String> {
        let output = self.run(&["log", "-1", "--pretty=%s", branch_name])?;
        Ok(output.trim().to_string())
    }

    pub fn has_uncommitted_changes(&self) -> Result<bool> {
        let output = self.run(&["status", "--porcelain"])?;
        Ok(!output.is_empty())
    }
}

/// Helper function to run a git command.
fn run_cmd<P: AsRef<Path>>(args: &[&str], cwd: P) -> Result<String> {
    let args_str = args.join(" ");
    info!(
        "Running git command: `git {}` in `{:?}`",
        args_str,
        cwd.as_ref()
    );

    let output = Command::new("git")
        .args(args)
        .current_dir(cwd.as_ref())
        .output()?;

    if output.status.success() {
        let stdout = String::from_utf8(output.stdout)?.trim().to_string();
        Ok(stdout)
    } else {
        let stderr = String::from_utf8(output.stderr)?;
        error!(
            "Git command `git {}` failed with stderr: {}",
            args_str, stderr
        );
        Err(anyhow!(TrainError::GitError { message: stderr }))
    }
}
