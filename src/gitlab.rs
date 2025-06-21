use crate::errors::TrainError;
use crate::utils::run_git_command;
use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

#[derive(Debug, Serialize, Deserialize)]
pub struct MergeRequest {
    pub id: u64,
    pub iid: u64,
    pub title: String,
    pub description: Option<String>,
    pub source_branch: String,
    pub target_branch: String,
    pub state: String,
    pub web_url: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GitLabProject {
    pub id: u64,
    pub name: String,
    pub path: String,
    pub namespace: GitLabNamespace,
    pub description: Option<String>,
    pub web_url: String,
    pub ssh_url_to_repo: String,
    pub http_url_to_repo: String,
    pub default_branch: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GitLabNamespace {
    pub id: u64,
    pub name: String,
    pub path: String,
}

#[derive(Debug, Serialize)]
pub struct CreateMergeRequestRequest {
    pub source_branch: String,
    pub target_branch: String,
    pub title: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ProjectInfo {
    pub host: String,
    pub namespace: String,
    pub project: String,
}

pub struct GitLabClient {
    client: Client,
    base_url: String,
    token: String,
    project_info: RwLock<Option<ProjectInfo>>,
    project_details: RwLock<Option<GitLabProject>>,
}

impl GitLabClient {
    pub async fn new() -> Result<Self> {
        let token = std::env::var("GITLAB_TOKEN").map_err(|_| TrainError::SecurityError {
            message: "GITLAB_TOKEN environment variable not set".to_string(),
        })?;

        let base_url =
            std::env::var("GITLAB_URL").unwrap_or_else(|_| "https://gitlab.com".to_string());

        let client = Client::new();

        Ok(Self {
            client,
            base_url,
            token,
            project_info: RwLock::new(None),
            project_details: RwLock::new(None),
        })
    }

    pub async fn detect_and_cache_project(&self) -> Result<GitLabProject> {
        // Check if project is already cached
        {
            let project_details = self.project_details.read().await;
            if let Some(ref project) = *project_details {
                return Ok(project.clone());
            }
        }

        // Try to auto-detect project from git remotes
        match self.detect_project_from_remotes().await {
            Ok((info, details)) => {
                // Cache both project info and details
                {
                    let mut project_info = self.project_info.write().await;
                    *project_info = Some(info);
                }
                {
                    let mut project_details = self.project_details.write().await;
                    *project_details = Some(details.clone());
                }
                Ok(details)
            }
            Err(_) => {
                // Fall back to environment variables if available
                if let Ok(project_id) = std::env::var("GITLAB_PROJECT_ID") {
                    if let Ok(project_details) = Self::get_project_by_id(
                        &self.base_url,
                        &self.token,
                        &self.client,
                        &project_id,
                    )
                    .await
                    {
                        // Cache the project details
                        {
                            let mut cached_details = self.project_details.write().await;
                            *cached_details = Some(project_details.clone());
                        }
                        return Ok(project_details);
                    }
                }

                Err(TrainError::GitLabError {
                    message:
                        "Could not detect GitLab project from git remotes or GITLAB_PROJECT_ID"
                            .to_string(),
                }
                .into())
            }
        }
    }

    async fn detect_project_from_remotes(&self) -> Result<(ProjectInfo, GitLabProject)> {
        // Get all git remotes
        let remotes_output = run_git_command(&["remote", "-v"])?;

        for line in remotes_output.lines() {
            if let Some(project_info) = Self::parse_gitlab_remote(line)? {
                // Verify this matches our GitLab instance
                if project_info.host == self.base_url.replace("https://", "").replace("http://", "")
                    || (self.base_url.contains("gitlab.com") && project_info.host == "gitlab.com")
                {
                    // Fetch project details from GitLab API
                    let project_path =
                        format!("{}/{}", project_info.namespace, project_info.project);
                    if let Ok(project_details) = Self::get_project_by_path(
                        &self.base_url,
                        &self.token,
                        &self.client,
                        &project_path,
                    )
                    .await
                    {
                        return Ok((project_info, project_details));
                    }
                }
            }
        }

        Err(TrainError::GitLabError {
            message: "Could not detect GitLab project from git remotes".to_string(),
        }
        .into())
    }

    fn parse_gitlab_remote(remote_line: &str) -> Result<Option<ProjectInfo>> {
        // Parse lines like:
        // origin  git@gitlab.com:namespace/project.git (fetch)
        // origin  https://gitlab.com/namespace/project.git (push)

        let parts: Vec<&str> = remote_line.split_whitespace().collect();
        if parts.len() < 2 {
            return Ok(None);
        }

        let url = parts[1];

        // Handle SSH URLs (git@host:namespace/project.git)
        if url.starts_with("git@") {
            if let Some(colon_pos) = url.find(':') {
                let host = &url[4..colon_pos]; // Skip "git@"
                let path = &url[colon_pos + 1..];
                let path = path.strip_suffix(".git").unwrap_or(path);

                if let Some(slash_pos) = path.find('/') {
                    let namespace = &path[..slash_pos];
                    let project = &path[slash_pos + 1..];

                    return Ok(Some(ProjectInfo {
                        host: host.to_string(),
                        namespace: namespace.to_string(),
                        project: project.to_string(),
                    }));
                }
            }
        }

        // Handle HTTPS URLs (https://host/namespace/project.git)
        if url.starts_with("http") {
            if let Ok(parsed_url) = url::Url::parse(url) {
                if let Some(host) = parsed_url.host_str() {
                    let path = parsed_url.path();
                    let path = path.strip_prefix('/').unwrap_or(path);
                    let path = path.strip_suffix(".git").unwrap_or(path);

                    if let Some(slash_pos) = path.find('/') {
                        let namespace = &path[..slash_pos];
                        let project = &path[slash_pos + 1..];

                        return Ok(Some(ProjectInfo {
                            host: host.to_string(),
                            namespace: namespace.to_string(),
                            project: project.to_string(),
                        }));
                    }
                }
            }
        }

        Ok(None)
    }

    async fn get_project_by_path(
        base_url: &str,
        token: &str,
        client: &Client,
        project_path: &str,
    ) -> Result<GitLabProject> {
        // URL encode the project path for the API
        let encoded_path = urlencoding::encode(project_path);
        let url = format!("{}/api/v4/projects/{}", base_url, encoded_path);

        let response = client
            .get(&url)
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await?;

        if response.status().is_success() {
            let project: GitLabProject = response.json().await?;
            Ok(project)
        } else {
            let error_text = response.text().await?;
            Err(TrainError::GitLabError {
                message: format!(
                    "Failed to get project by path {}: {}",
                    project_path, error_text
                ),
            }
            .into())
        }
    }

    async fn get_project_by_id(
        base_url: &str,
        token: &str,
        client: &Client,
        project_id: &str,
    ) -> Result<GitLabProject> {
        let url = format!("{}/api/v4/projects/{}", base_url, project_id);

        let response = client
            .get(&url)
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await?;

        if response.status().is_success() {
            let project: GitLabProject = response.json().await?;
            Ok(project)
        } else {
            let error_text = response.text().await?;
            Err(TrainError::GitLabError {
                message: format!("Failed to get project by ID {}: {}", project_id, error_text),
            }
            .into())
        }
    }

    async fn get_project_id_for_api(&self) -> Result<String> {
        // Try to get cached project details first
        {
            let project_details = self.project_details.read().await;
            if let Some(ref project) = *project_details {
                return Ok(project.id.to_string());
            }
        }

        // If not cached, detect and cache the project
        let project = self.detect_and_cache_project().await?;
        Ok(project.id.to_string())
    }

    pub async fn create_merge_request(
        &self,
        request: CreateMergeRequestRequest,
    ) -> Result<MergeRequest> {
        let project_id = self.get_project_id_for_api().await?;
        let url = format!(
            "{}/api/v4/projects/{}/merge_requests",
            self.base_url, project_id
        );

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&request)
            .send()
            .await?;

        if response.status().is_success() {
            let mr: MergeRequest = response.json().await?;
            Ok(mr)
        } else {
            let error_text = response.text().await?;
            Err(TrainError::GitLabError {
                message: format!("Failed to create MR: {}", error_text),
            }
            .into())
        }
    }

    pub async fn update_merge_request(
        &self,
        iid: u64,
        title: Option<String>,
        description: Option<String>,
    ) -> Result<MergeRequest> {
        let project_id = self.get_project_id_for_api().await?;
        let url = format!(
            "{}/api/v4/projects/{}/merge_requests/{}",
            self.base_url, project_id, iid
        );

        let mut params = serde_json::Map::new();
        if let Some(title) = title {
            params.insert("title".to_string(), serde_json::Value::String(title));
        }
        if let Some(description) = description {
            params.insert(
                "description".to_string(),
                serde_json::Value::String(description),
            );
        }

        let response = self
            .client
            .put(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&params)
            .send()
            .await?;

        if response.status().is_success() {
            let mr: MergeRequest = response.json().await?;
            Ok(mr)
        } else {
            let error_text = response.text().await?;
            Err(TrainError::GitLabError {
                message: format!("Failed to update MR: {}", error_text),
            }
            .into())
        }
    }

    /// Update merge request with optional target branch change
    pub async fn update_merge_request_with_target(
        &self,
        iid: u64,
        title: Option<String>,
        description: Option<String>,
        target_branch: Option<String>,
    ) -> Result<MergeRequest> {
        let project_id = self.get_project_id_for_api().await?;
        let url = format!(
            "{}/api/v4/projects/{}/merge_requests/{}",
            self.base_url, project_id, iid
        );

        let mut params = serde_json::Map::new();
        if let Some(title) = title {
            params.insert("title".to_string(), serde_json::Value::String(title));
        }
        if let Some(description) = description {
            params.insert(
                "description".to_string(),
                serde_json::Value::String(description),
            );
        }
        if let Some(target_branch) = target_branch {
            params.insert(
                "target_branch".to_string(),
                serde_json::Value::String(target_branch),
            );
        }

        let response = self
            .client
            .put(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&params)
            .send()
            .await?;

        if response.status().is_success() {
            let mr: MergeRequest = response.json().await?;
            Ok(mr)
        } else {
            let error_text = response.text().await?;
            Err(TrainError::GitLabError {
                message: format!("Failed to update MR with target: {}", error_text),
            }
            .into())
        }
    }

    /// Get the current state of a merge request
    pub async fn get_merge_request(&self, iid: u64) -> Result<MergeRequest> {
        let project_id = self.get_project_id_for_api().await?;
        let url = format!(
            "{}/api/v4/projects/{}/merge_requests/{}",
            self.base_url, project_id, iid
        );

        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .send()
            .await?;

        if response.status().is_success() {
            let mr: MergeRequest = response.json().await?;
            Ok(mr)
        } else {
            let error_text = response.text().await?;
            Err(TrainError::GitLabError {
                message: format!("Failed to get MR: {}", error_text),
            }
            .into())
        }
    }
}
