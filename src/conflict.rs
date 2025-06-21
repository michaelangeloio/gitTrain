use anyhow::Result;
use regex::Regex;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::NamedTempFile;
use tracing::{info, warn, error};

use crate::config::{TrainConfig, AutoResolveStrategy, RebaseStrategy};
use crate::errors::TrainError;
use crate::utils::{run_git_command, print_success, print_warning, print_error, print_info, confirm_action};

#[derive(Debug, Clone)]
pub struct ConflictInfo {
    pub files: Vec<ConflictFile>,
    pub conflict_type: ConflictType,
    pub can_auto_resolve: bool,
}

#[derive(Debug, Clone)]
pub struct ConflictFile {
    pub path: String,
    pub status: ConflictStatus,
    pub conflict_markers: Vec<ConflictMarker>,
}

#[derive(Debug, Clone)]
pub struct ConflictMarker {
    pub line_number: usize,
    pub marker_type: ConflictMarkerType,
    pub content: String,
}

#[derive(Debug, Clone)]
pub enum ConflictStatus {
    BothModified,
    AddedByUs,
    AddedByThem,
    DeletedByUs,
    DeletedByThem,
}

#[derive(Debug, Clone)]
pub enum ConflictMarkerType {
    Ours,      // <<<<<<< HEAD
    Theirs,    // >>>>>>> commit
    Separator, // =======
}

#[derive(Debug, Clone)]
pub enum ConflictType {
    Rebase,
    Merge,
    CherryPick,
    Unknown,
}

#[derive(Debug, Clone)]
pub enum GitState {
    Clean,
    Rebasing,
    Merging,
    CherryPicking,
    Conflicted,
}

pub struct ConflictResolver {
    config: TrainConfig,
    git_dir: PathBuf,
}

impl ConflictResolver {
    pub fn new(config: TrainConfig, git_dir: PathBuf) -> Self {
        Self { config, git_dir }
    }

    /// Check the current git state for conflicts
    pub fn get_git_state(&self) -> Result<GitState> {
        let git_dir = &self.git_dir;
        
        if git_dir.join("REBASE_HEAD").exists() {
            return Ok(GitState::Rebasing);
        }
        
        if git_dir.join("MERGE_HEAD").exists() {
            return Ok(GitState::Merging);
        }
        
        if git_dir.join("CHERRY_PICK_HEAD").exists() {
            return Ok(GitState::CherryPicking);
        }
        
        // Check for conflicts in working directory
        let status_output = run_git_command(&["status", "--porcelain"])?;
        for line in status_output.lines() {
            if line.starts_with("UU") || line.starts_with("AA") || 
               line.starts_with("DU") || line.starts_with("UD") ||
               line.starts_with("AU") || line.starts_with("UA") {
                return Ok(GitState::Conflicted);
            }
        }
        
        Ok(GitState::Clean)
    }

    /// Detect and analyze conflicts in the repository
    pub fn detect_conflicts(&self) -> Result<Option<ConflictInfo>> {
        let git_state = self.get_git_state()?;
        
        match git_state {
            GitState::Clean => Ok(None),
            GitState::Rebasing => self.analyze_rebase_conflicts(),
            GitState::Merging => self.analyze_merge_conflicts(),
            GitState::CherryPicking => self.analyze_cherry_pick_conflicts(),
            GitState::Conflicted => self.analyze_working_directory_conflicts(),
        }
    }

    /// Attempt to resolve conflicts automatically based on configuration
    pub async fn auto_resolve_conflicts(&self, conflict_info: &ConflictInfo) -> Result<bool> {
        match self.config.conflict_resolution.auto_resolve_strategy {
            AutoResolveStrategy::Never => {
                print_info("Auto-resolution disabled, will prompt for manual resolution");
                Ok(false)
            },
            AutoResolveStrategy::Simple => {
                self.attempt_simple_resolution(conflict_info).await
            },
            AutoResolveStrategy::Smart => {
                self.attempt_smart_resolution(conflict_info).await
            },
        }
    }

    /// Handle conflicts with user intervention
    pub async fn resolve_conflicts_interactively(&self, conflict_info: &ConflictInfo) -> Result<()> {
        print_info("Conflicts detected. Opening editor for manual resolution...");
        
        // Show conflict summary
        self.print_conflict_summary(conflict_info);
        
        // Ask user how they want to proceed
        let options = vec![
            "Open editor to resolve conflicts manually",
            "Try automatic resolution",
            "Abort current operation",
        ];
        
        let choice = crate::utils::select_from_list(&options, "How would you like to resolve the conflicts?")?;
        
        match choice {
            0 => self.open_editor_for_conflicts(conflict_info).await,
            1 => {
                if self.auto_resolve_conflicts(conflict_info).await? {
                    print_success("Conflicts resolved automatically");
                    Ok(())
                } else {
                    print_warning("Automatic resolution failed, falling back to manual resolution");
                    self.open_editor_for_conflicts(conflict_info).await
                }
            },
            2 => self.abort_current_operation(),
            _ => unreachable!(),
        }
    }

    /// Open the configured editor for manual conflict resolution
    async fn open_editor_for_conflicts(&self, conflict_info: &ConflictInfo) -> Result<()> {
        let editor_config = &self.config.editor;
        
        for conflict_file in &conflict_info.files {
            print_info(&format!("Opening {} in {}", conflict_file.path, editor_config.default_editor));
            
            let mut cmd = Command::new(&editor_config.default_editor);
            cmd.args(&editor_config.editor_args);
            cmd.arg(&conflict_file.path);
            
            let status = cmd.status()?;
            
            if !status.success() {
                return Err(TrainError::GitError {
                    message: format!("Editor {} exited with error", editor_config.default_editor),
                }.into());
            }
        }
        
        // Verify conflicts are resolved
        self.verify_conflicts_resolved().await
    }

    /// Verify that all conflicts have been resolved
    pub async fn verify_conflicts_resolved(&self) -> Result<()> {
        let current_conflicts = self.detect_conflicts()?;
        
        if let Some(conflicts) = current_conflicts {
            print_error(&format!("Still have {} unresolved conflicts", conflicts.files.len()));
            
            if confirm_action("Do you want to continue editing?")? {
                return Box::pin(self.open_editor_for_conflicts(&conflicts)).await;
            } else {
                return Err(TrainError::InvalidState {
                    message: "Conflicts not resolved".to_string(),
                }.into());
            }
        }
        
        // Add resolved files and continue
        run_git_command(&["add", "."])?;
        
        match self.get_git_state()? {
            GitState::Rebasing => {
                run_git_command(&["rebase", "--continue"])?;
                print_success("Rebase continued successfully");
            },
            GitState::Merging => {
                run_git_command(&["commit", "--no-edit"])?;
                print_success("Merge completed successfully");
            },
            GitState::CherryPicking => {
                run_git_command(&["cherry-pick", "--continue"])?;
                print_success("Cherry-pick continued successfully");
            },
            _ => {
                print_success("Conflicts resolved");
            }
        }
        
        Ok(())
    }

    /// Attempt simple automatic conflict resolution
    async fn attempt_simple_resolution(&self, conflict_info: &ConflictInfo) -> Result<bool> {
        let mut resolved_count = 0;
        
        for conflict_file in &conflict_info.files {
            if self.can_resolve_simple(&conflict_file)? {
                if self.resolve_simple_conflict(&conflict_file)? {
                    resolved_count += 1;
                    print_success(&format!("Auto-resolved simple conflict in {}", conflict_file.path));
                }
            }
        }
        
        if resolved_count == conflict_info.files.len() {
            run_git_command(&["add", "."])?;
            Ok(true)
        } else {
            print_warning(&format!("Only resolved {} of {} conflicts", resolved_count, conflict_info.files.len()));
            Ok(false)
        }
    }

    /// Attempt smart automatic conflict resolution
    async fn attempt_smart_resolution(&self, conflict_info: &ConflictInfo) -> Result<bool> {
        // First try simple resolution
        if self.attempt_simple_resolution(conflict_info).await? {
            return Ok(true);
        }
        
        // Try more sophisticated resolution strategies
        let mut resolved_count = 0;
        
        for conflict_file in &conflict_info.files {
            if self.can_resolve_smart(&conflict_file)? {
                if self.resolve_smart_conflict(&conflict_file)? {
                    resolved_count += 1;
                    print_success(&format!("Auto-resolved conflict in {}", conflict_file.path));
                }
            }
        }
        
        if resolved_count == conflict_info.files.len() {
            run_git_command(&["add", "."])?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Check if a conflict can be resolved with simple strategies
    fn can_resolve_simple(&self, conflict_file: &ConflictFile) -> Result<bool> {
        let content = fs::read_to_string(&conflict_file.path)?;
        
        // Simple cases we can auto-resolve:
        // 1. Whitespace-only conflicts
        // 2. Line ending conflicts
        // 3. Import/include order conflicts
        
        for marker in &conflict_file.conflict_markers {
            if !self.is_whitespace_conflict(&content, marker)? &&
               !self.is_line_ending_conflict(&content, marker)? &&
               !self.is_import_order_conflict(&content, marker)? {
                return Ok(false);
            }
        }
        
        Ok(true)
    }

    /// Check if a conflict can be resolved with smart strategies
    fn can_resolve_smart(&self, conflict_file: &ConflictFile) -> Result<bool> {
        let content = fs::read_to_string(&conflict_file.path)?;
        
        // Smart resolution strategies:
        // 1. All simple cases
        // 2. Non-overlapping changes
        // 3. Clear preference patterns (e.g., always take newer version)
        
        if self.can_resolve_simple(conflict_file)? {
            return Ok(true);
        }
        
        // Check for non-overlapping changes
        self.has_non_overlapping_changes(&content, conflict_file)
    }

    fn analyze_rebase_conflicts(&self) -> Result<Option<ConflictInfo>> {
        self.analyze_conflicts_by_type(ConflictType::Rebase)
    }

    fn analyze_merge_conflicts(&self) -> Result<Option<ConflictInfo>> {
        self.analyze_conflicts_by_type(ConflictType::Merge)
    }

    fn analyze_cherry_pick_conflicts(&self) -> Result<Option<ConflictInfo>> {
        self.analyze_conflicts_by_type(ConflictType::CherryPick)
    }

    fn analyze_working_directory_conflicts(&self) -> Result<Option<ConflictInfo>> {
        self.analyze_conflicts_by_type(ConflictType::Unknown)
    }

    fn analyze_conflicts_by_type(&self, conflict_type: ConflictType) -> Result<Option<ConflictInfo>> {
        let status_output = run_git_command(&["status", "--porcelain"])?;
        let mut conflict_files = Vec::new();
        
        for line in status_output.lines() {
            if let Some(conflict_file) = self.parse_conflict_line(line)? {
                conflict_files.push(conflict_file);
            }
        }
        
        if conflict_files.is_empty() {
            return Ok(None);
        }
        
        let can_auto_resolve = conflict_files.iter()
            .all(|f| self.can_resolve_simple(f).unwrap_or(false));
        
        Ok(Some(ConflictInfo {
            files: conflict_files,
            conflict_type,
            can_auto_resolve,
        }))
    }

    fn parse_conflict_line(&self, line: &str) -> Result<Option<ConflictFile>> {
        if line.len() < 3 {
            return Ok(None);
        }
        
        let status_chars: Vec<char> = line.chars().take(2).collect();
        let file_path = line[3..].to_string();
        
        let status = match (status_chars[0], status_chars[1]) {
            ('U', 'U') => ConflictStatus::BothModified,
            ('A', 'U') => ConflictStatus::AddedByUs,
            ('U', 'A') => ConflictStatus::AddedByThem,
            ('D', 'U') => ConflictStatus::DeletedByUs,
            ('U', 'D') => ConflictStatus::DeletedByThem,
            _ => return Ok(None),
        };
        
        let conflict_markers = self.find_conflict_markers(&file_path)?;
        
        Ok(Some(ConflictFile {
            path: file_path,
            status,
            conflict_markers,
        }))
    }

    fn find_conflict_markers(&self, file_path: &str) -> Result<Vec<ConflictMarker>> {
        let content = match fs::read_to_string(file_path) {
            Ok(content) => content,
            Err(_) => return Ok(Vec::new()), // File might be deleted
        };
        
        let mut markers = Vec::new();
        let lines: Vec<&str> = content.lines().collect();
        
        for (line_num, line) in lines.iter().enumerate() {
            if line.starts_with("<<<<<<<") {
                markers.push(ConflictMarker {
                    line_number: line_num + 1,
                    marker_type: ConflictMarkerType::Ours,
                    content: line.to_string(),
                });
            } else if line.starts_with("=======") {
                markers.push(ConflictMarker {
                    line_number: line_num + 1,
                    marker_type: ConflictMarkerType::Separator,
                    content: line.to_string(),
                });
            } else if line.starts_with(">>>>>>>") {
                markers.push(ConflictMarker {
                    line_number: line_num + 1,
                    marker_type: ConflictMarkerType::Theirs,
                    content: line.to_string(),
                });
            }
        }
        
        Ok(markers)
    }

    pub fn print_conflict_summary(&self, conflict_info: &ConflictInfo) {
        print_warning(&format!("Found {} conflicted files:", conflict_info.files.len()));
        
        for conflict_file in &conflict_info.files {
            println!("  📄 {} ({:?})", conflict_file.path, conflict_file.status);
            if !conflict_file.conflict_markers.is_empty() {
                println!("     {} conflict markers found", conflict_file.conflict_markers.len());
            }
        }
        
        if conflict_info.can_auto_resolve {
            print_info("These conflicts may be automatically resolvable");
        } else {
            print_warning("Manual resolution may be required");
        }
    }

    pub fn abort_current_operation(&self) -> Result<()> {
        match self.get_git_state()? {
            GitState::Rebasing => {
                run_git_command(&["rebase", "--abort"])?;
                print_info("Rebase aborted");
            },
            GitState::Merging => {
                run_git_command(&["merge", "--abort"])?;
                print_info("Merge aborted");
            },
            GitState::CherryPicking => {
                run_git_command(&["cherry-pick", "--abort"])?;
                print_info("Cherry-pick aborted");
            },
            _ => {
                print_warning("No operation to abort");
            }
        }
        Ok(())
    }

    // Placeholder implementations for conflict resolution strategies
    fn resolve_simple_conflict(&self, _conflict_file: &ConflictFile) -> Result<bool> {
        // TODO: Implement simple conflict resolution
        Ok(false)
    }

    fn resolve_smart_conflict(&self, _conflict_file: &ConflictFile) -> Result<bool> {
        // TODO: Implement smart conflict resolution
        Ok(false)
    }

    fn is_whitespace_conflict(&self, _content: &str, _marker: &ConflictMarker) -> Result<bool> {
        // TODO: Implement whitespace conflict detection
        Ok(false)
    }

    fn is_line_ending_conflict(&self, _content: &str, _marker: &ConflictMarker) -> Result<bool> {
        // TODO: Implement line ending conflict detection
        Ok(false)
    }

    fn is_import_order_conflict(&self, _content: &str, _marker: &ConflictMarker) -> Result<bool> {
        // TODO: Implement import order conflict detection
        Ok(false)
    }

    fn has_non_overlapping_changes(&self, _content: &str, _conflict_file: &ConflictFile) -> Result<bool> {
        // TODO: Implement non-overlapping change detection
        Ok(false)
    }
} 