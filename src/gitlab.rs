use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use crate::errors::TrainError;

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

#[derive(Debug, Serialize)]
pub struct CreateMergeRequestRequest {
    pub source_branch: String,
    pub target_branch: String,
    pub title: String,
    pub description: Option<String>,
}

pub struct GitLabClient {
    client: Client,
    base_url: String,
    token: String,
    project_id: String,
}

impl GitLabClient {
    pub async fn new() -> Result<Self> {
        let token = std::env::var("GITLAB_TOKEN")
            .map_err(|_| TrainError::SecurityError {
                message: "GITLAB_TOKEN environment variable not set".to_string(),
            })?;
            
        let base_url = std::env::var("GITLAB_URL")
            .unwrap_or_else(|_| "https://gitlab.com".to_string());
            
        let project_id = std::env::var("GITLAB_PROJECT_ID")
            .map_err(|_| TrainError::SecurityError {
                message: "GITLAB_PROJECT_ID environment variable not set".to_string(),
            })?;

        let client = Client::new();

        Ok(Self {
            client,
            base_url,
            token,
            project_id,
        })
    }

    pub async fn create_merge_request(&self, request: CreateMergeRequestRequest) -> Result<MergeRequest> {
        let url = format!("{}/api/v4/projects/{}/merge_requests", self.base_url, self.project_id);
        
        let response = self.client
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
            }.into())
        }
    }

    pub async fn get_merge_requests(&self) -> Result<Vec<MergeRequest>> {
        let url = format!("{}/api/v4/projects/{}/merge_requests", self.base_url, self.project_id);
        
        let response = self.client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .query(&[("state", "opened")])
            .send()
            .await?;

        if response.status().is_success() {
            let mrs: Vec<MergeRequest> = response.json().await?;
            Ok(mrs)
        } else {
            let error_text = response.text().await?;
            Err(TrainError::GitLabError {
                message: format!("Failed to get MRs: {}", error_text),
            }.into())
        }
    }

    pub async fn update_merge_request(&self, iid: u64, title: Option<String>, description: Option<String>) -> Result<MergeRequest> {
        let url = format!("{}/api/v4/projects/{}/merge_requests/{}", self.base_url, self.project_id, iid);
        
        let mut params = serde_json::Map::new();
        if let Some(title) = title {
            params.insert("title".to_string(), serde_json::Value::String(title));
        }
        if let Some(description) = description {
            params.insert("description".to_string(), serde_json::Value::String(description));
        }

        let response = self.client
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
            }.into())
        }
    }
} 