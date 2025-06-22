use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::gitlab::api::GitLabProject;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackBranch {
    pub name: String,
    pub parent: Option<String>,
    pub children: Vec<String>,
    pub commit_hash: String,
    pub mr_iid: Option<u64>,
    pub mr_title: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stack {
    pub id: String,
    pub name: String,
    pub base_branch: String,
    pub branches: HashMap<String, StackBranch>,
    pub current_branch: Option<String>,
    pub gitlab_project: Option<GitLabProject>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
