// tests/integration_tests.rs

use anyhow::Result;
use gittrain::config::TrainConfig;
use gittrain::git::GitRepository;
use gittrain::gitlab::{
    CreateMergeRequestRequest, GitLabApi, GitLabNamespace, GitLabProject, MergeRequest,
};
use gittrain::stack::StackManager;
use std::collections::HashMap;
use std::fs;

use std::process::Command;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tokio::sync::RwLock;
use gittrain::config::AutoResolveStrategy;

// A helper struct for managing a temporary git repository for tests
struct TestRepo {
    dir: TempDir,
    remote_dir: TempDir,
    repo: GitRepository,
}

impl TestRepo {
    fn new() -> Result<Self> {
        // Setup the "local" repository
        let local_tmp = tempfile::tempdir()?;
        let local_path = local_tmp.path();

        Command::new("git").arg("init").arg("-b").arg("main").current_dir(local_path).output()?;
        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(local_path)
            .output()?;
        Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(local_path)
            .output()?;

        let repo = GitRepository::new(local_path)?;
        repo.run(&["commit", "--allow-empty", "-m", "initial commit"])?;

        // Setup the "remote" repository
        let remote_tmp = tempfile::tempdir()?;
        let remote_path = remote_tmp.path();
        Command::new("git").arg("init").arg("--bare").current_dir(remote_path).output()?;
        
        // Add the remote to the local repo
        repo.run(&["remote", "add", "origin", remote_path.to_str().unwrap()])?;
        repo.run(&["push", "-u", "origin", "main"])?;


        Ok(Self { dir: local_tmp, remote_dir: remote_tmp, repo })
    }

    fn path(&self) -> &std::path::Path {
        self.dir.path()
    }

    fn git_repo(&self) -> &GitRepository {
        &self.repo
    }

    fn commit(&self, message: &str) -> Result<String> {
        self.repo.run(&["commit", "--allow-empty", "-m", message])?;
        self.repo.get_current_commit_hash()
    }

    fn create_branch(&self, name: &str) -> Result<()> {
        self.repo.run(&["checkout", "-b", name])?;
        Ok(())
    }

    fn checkout(&self, name: &str) -> Result<()> {
        self.repo.run(&["checkout", name])?;
        Ok(())
    }

    fn create_file(&self, name: &str, content: &str) -> Result<()> {
        fs::write(self.path().join(name), content)?;
        self.repo.run(&["add", name])?;
        Ok(())
    }

    fn run(&self, args: &[&str]) -> Result<()> {
        self.repo.run(args)?;
        Ok(())
    }
}

// Mock GitLab client for testing
#[derive(Clone)]
struct MockGitLab {
    project: GitLabProject,
    project_details: Arc<RwLock<Option<GitLabProject>>>,
    merge_requests: Arc<Mutex<HashMap<u64, MergeRequest>>>,
    next_mr_iid: Arc<Mutex<u64>>,
}

impl MockGitLab {
    fn new() -> Self {
        let project = GitLabProject {
            id: 1,
            name: "test-project".to_string(),
            path: "test-project".to_string(),
            namespace: GitLabNamespace {
                id: 1,
                name: "test-namespace".to_string(),
                path: "test-namespace".to_string(),
            },
            description: None,
            web_url: "http://gitlab.com/test-namespace/test-project".to_string(),
            ssh_url_to_repo: "".to_string(),
            http_url_to_repo: "".to_string(),
            default_branch: "main".to_string(),
        };
        Self {
            project: project.clone(),
            project_details: Arc::new(RwLock::new(Some(project))),
            merge_requests: Arc::new(Mutex::new(HashMap::new())),
            next_mr_iid: Arc::new(Mutex::new(1)),
        }
    }
}

#[async_trait::async_trait]
impl GitLabApi for MockGitLab {
    async fn detect_and_cache_project(&self) -> Result<GitLabProject> {
        Ok(self.project.clone())
    }

    async fn create_merge_request(
        &self,
        request: CreateMergeRequestRequest,
    ) -> Result<MergeRequest> {
        let mut iid = self.next_mr_iid.lock().unwrap();
        let new_iid = *iid;
        *iid += 1;

        let mr = MergeRequest {
            id: new_iid,
            iid: new_iid,
            title: request.title,
            description: request.description,
            source_branch: request.source_branch,
            target_branch: request.target_branch,
            state: "opened".to_string(),
            web_url: format!("{}/merge_requests/{}", self.project.web_url, new_iid),
        };

        self.merge_requests.lock().unwrap().insert(new_iid, mr.clone());
        Ok(mr)
    }

    async fn update_merge_request(
        &self,
        iid: u64,
        title: Option<String>,
        description: Option<String>,
    ) -> Result<MergeRequest> {
        let mut mrs = self.merge_requests.lock().unwrap();
        let mr = mrs.get_mut(&iid).unwrap();

        if let Some(title) = title {
            mr.title = title;
        }
        if let Some(description) = description {
            mr.description = Some(description);
        }

        Ok(mr.clone())
    }
    
    async fn update_merge_request_with_target(
        &self,
        iid: u64,
        title: Option<String>,
        description: Option<String>,
        target_branch: Option<String>,
    ) -> Result<MergeRequest> {
        let mut mrs = self.merge_requests.lock().unwrap();
        let mr = mrs.get_mut(&iid).unwrap();

        if let Some(title) = title {
            mr.title = title;
        }
        if let Some(description) = description {
            mr.description = Some(description);
        }
        if let Some(target_branch) = target_branch {
            mr.target_branch = target_branch;
        }

        Ok(mr.clone())
    }

    async fn get_merge_request(&self, iid: u64) -> Result<MergeRequest> {
        let mrs = self.merge_requests.lock().unwrap();
        Ok(mrs.get(&iid).cloned().unwrap())
    }

    fn get_project_details(&self) -> &RwLock<Option<GitLabProject>> {
        &self.project_details
    }
}


#[cfg(test)]
mod integration_tests {
    use super::*;

    async fn setup() -> Result<(TestRepo, StackManager, Arc<Mutex<HashMap<u64, MergeRequest>>>)> {
        let test_repo = TestRepo::new()?;
        let mut config = TrainConfig::default();
        config.conflict_resolution.auto_force_push_after_rebase = true;
        config.git.verify_signatures = false;
        config.editor.default_editor = "true".to_string();

        let mock_gitlab = MockGitLab::new();
        let mrs = mock_gitlab.merge_requests.clone();

        let mut stack_manager =
            StackManager::new_with_git_repo(config.clone(), test_repo.git_repo().clone()).await?;
        stack_manager.set_gitlab_client(Box::new(mock_gitlab));

        Ok((test_repo, stack_manager, mrs))
    }

    #[tokio::test]
    async fn test_create_stack_and_sync() -> Result<()> {
        let (test_repo, mut stack_manager, mrs) = setup().await?;

        // 1. Create a feature branch
        test_repo.create_branch("feature-1")?;
        test_repo.create_file("file1.txt", "content")?;
        test_repo.commit("feat: add file1")?;

        // 2. Create a stack
        stack_manager.create_stack("my-stack").await?;

        // 3. Sync the stack by pushing
        stack_manager.push_stack().await?;

        // 4. Assertions
        let mrs = mrs.lock().unwrap();
        assert_eq!(mrs.len(), 1);
        let mr = mrs.values().next().unwrap();
        assert_eq!(mr.title, "[Stack: my-stack] feat: add file1");
        assert_eq!(mr.source_branch, "feature-1");
        assert_eq!(mr.target_branch, "main");

        Ok(())
    }

    #[tokio::test]
    async fn test_update_middle_of_stack() -> Result<()> {
        let (test_repo, mut stack_manager, mrs) = setup().await?;

        // 1. Setup a stack of 2 feature branches that modify different files
        test_repo.create_branch("feature-1")?;
        test_repo.create_file("file1.txt", "content1")?;
        let feature1_initial_hash = test_repo.commit("feat: add file1")?;
        stack_manager.create_stack("my-stack").await?;

        // Create feature-2 from main to avoid inheriting file1.txt
        test_repo.checkout("main")?;
        test_repo.create_branch("feature-2")?;
        test_repo.create_file("file2.txt", "content2")?;
        test_repo.commit("feat: add file2")?;
        stack_manager.add_branch_to_stack(Some("feature-1")).await?;

        // 2. Push the stack to create MRs
        stack_manager.push_stack().await?;
        assert_eq!(mrs.lock().unwrap().len(), 2);

        // 3. Go back to feature-1 and amend it. This will also rebase feature-2.
        test_repo.checkout("feature-1")?;
        test_repo.create_file("file1.txt", "new-content1")?;
        stack_manager.amend_changes(Some("feat: update file1")).await?;

        // 4. Push again to update the remote MRs
        stack_manager.push_stack().await?;

        // 5. Assert that feature-2 was rebased and MRs updated
        let parent_of_feature2 = test_repo.git_repo().run(&["rev-parse", "feature-2^"])?;
        let feature1_hash_after_amend = test_repo
            .git_repo()
            .get_commit_hash_for_branch("feature-1")?;
        assert_ne!(
            feature1_initial_hash.trim(),
            feature1_hash_after_amend.trim()
        );
        assert_eq!(parent_of_feature2.trim(), feature1_hash_after_amend.trim());

        let mrs_after = mrs.lock().unwrap();
        assert_eq!(mrs_after.len(), 2);
        
        let mr_feature1 = mrs_after.values().find(|mr| mr.source_branch == "feature-1").unwrap();
        let mr_feature2 = mrs_after.values().find(|mr| mr.source_branch == "feature-2").unwrap();

        assert_eq!(mr_feature1.title, "[Stack: my-stack] feat: update file1");
        assert_eq!(mr_feature2.title, "[Stack: my-stack] feat: add file2");
        assert_eq!(mr_feature2.target_branch, "feature-1");

        Ok(())
    }

    #[tokio::test]
    async fn test_edit_earlier_branch_from_latest() -> Result<()> {
        let (test_repo, mut stack_manager, mrs) = setup().await?;

        // 1. Create a stack of 3 branches with different files
        test_repo.create_branch("feature-1")?;
        test_repo.create_file("file1.txt", "original content from feature-1")?;
        test_repo.commit("feat: add file1 in feature-1")?;
        stack_manager.create_stack("my-stack").await?;

        test_repo.create_branch("feature-2")?;
        test_repo.create_file("file2.txt", "content from feature-2")?;
        test_repo.commit("feat: add file2 in feature-2")?;
        stack_manager.add_branch_to_stack(Some("feature-1")).await?;

        test_repo.create_branch("feature-3")?;
        test_repo.create_file("file3.txt", "content from feature-3")?;
        test_repo.commit("feat: add file3 in feature-3")?;
        stack_manager.add_branch_to_stack(Some("feature-2")).await?;

        // 2. Push the stack to create MRs
        stack_manager.push_stack().await?;
        assert_eq!(mrs.lock().unwrap().len(), 3);

        // 3. Stay on feature-3 (the latest branch) and edit a file from feature-1
        // This simulates the common workflow where you're working on the latest branch
        // but realize you need to fix something from an earlier branch
        test_repo.create_file("file1.txt", "updated content from feature-1 (edited from feature-3)")?;
        // Don't commit yet - let amend_changes handle it
        
        // 4. Use existing functionality to sync the stack - it should automatically detect
        // that we've edited files from an earlier branch and handle it properly
        stack_manager.amend_changes(Some("fix: update file1 from earlier branch")).await?;

        // 5. Verify that:
        // - feature-1 has the updated file1.txt content
        // - feature-2 and feature-3 were rebased onto the updated feature-1
        // - The stack hierarchy is maintained

        // Check feature-1 has the updated content
        test_repo.checkout("feature-1")?;
        let file1_content = std::fs::read_to_string(test_repo.path().join("file1.txt"))?;
        assert_eq!(file1_content, "updated content from feature-1 (edited from feature-3)");

        // Check that feature-2 was rebased (parent should be updated feature-1)
        test_repo.checkout("feature-2")?;
        let feature2_parent = test_repo.git_repo().run(&["rev-parse", "feature-2^"])?;
        let feature1_hash = test_repo.git_repo().get_commit_hash_for_branch("feature-1")?;
        assert_eq!(feature2_parent.trim(), feature1_hash.trim());

        // Check that feature-3 was rebased (parent should be updated feature-2)
        test_repo.checkout("feature-3")?;
        let feature3_parent = test_repo.git_repo().run(&["rev-parse", "feature-3^"])?;
        let feature2_hash = test_repo.git_repo().get_commit_hash_for_branch("feature-2")?;
        assert_eq!(feature3_parent.trim(), feature2_hash.trim());

        // Verify all files are present in feature-3
        assert!(test_repo.path().join("file1.txt").exists());
        assert!(test_repo.path().join("file2.txt").exists());
        assert!(test_repo.path().join("file3.txt").exists());

        // Check that file1.txt has the updated content in feature-3
        let file1_content_in_feature3 = std::fs::read_to_string(test_repo.path().join("file1.txt"))?;
        assert_eq!(file1_content_in_feature3, "updated content from feature-1 (edited from feature-3)");

        // 6. Push the updated stack
        stack_manager.push_stack().await?;

        // Verify MRs are updated
        let mrs_after = mrs.lock().unwrap();
        assert_eq!(mrs_after.len(), 3);

        Ok(())
    }

    #[tokio::test]
    async fn test_conflict_detection() -> Result<()> {
        // Custom setup for this test to control config
        let test_repo = TestRepo::new()?;
        let mut config = TrainConfig::default();
        config.conflict_resolution.auto_resolve_strategy = AutoResolveStrategy::Never;
        config.conflict_resolution.auto_force_push_after_rebase = true;
        config.git.verify_signatures = false;
        config.editor.default_editor = "true".to_string();
        let mock_gitlab = MockGitLab::new();
        let _mrs = mock_gitlab.merge_requests.clone();

        let mut stack_manager =
            StackManager::new_with_git_repo(config, test_repo.git_repo().clone()).await?;
        stack_manager.set_gitlab_client(Box::new(mock_gitlab));

        // 1. Setup a stack
        test_repo.create_branch("feature-1")?;
        test_repo.create_file("file.txt", "line 1\nline 2\nline 3")?;
        test_repo.commit("feat: add file")?;
        stack_manager.create_stack("my-stack").await?;

        // 2. Modify file on main to create a conflict on a specific line
        test_repo.checkout("main")?;
        test_repo.create_file("file.txt", "line 1\nline 2 - main\nline 3")?;
        test_repo.commit("refactor: update file on main")?;
        test_repo.run(&["push", "origin", "main"])?;

        // 3. Modify the same line on the feature branch
        test_repo.checkout("feature-1")?;
        test_repo.create_file("file.txt", "line 1\nline 2 - feature\nline 3")?;
        test_repo.commit("feat: update file on feature")?;

        // 4. Try to sync by propagating changes from main, which should cause a conflict
        let result = stack_manager.sync_with_remote().await;

        // 5. Assert that a conflict was detected and the correct error is returned
        assert!(result.is_err());
        if let Some(err) = result.err() {
            let err_string = err.to_string();
            assert!(err_string.contains("Manual conflict resolution required"));
            assert!(err_string.contains("rebase of feature-1 onto main"));
        }

        // 6. Check git state
        let conflict_resolver = stack_manager.get_conflict_resolver();
        let state = conflict_resolver.get_git_state()?;
        assert!(matches!(state, gittrain::conflict::GitState::Conflicted));

        Ok(())
    }
} 