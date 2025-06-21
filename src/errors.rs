use thiserror::Error;

#[derive(Error, Debug)]
pub enum TrainError {
    #[error("Git operation failed: {message}")]
    GitError { message: String },
    
    #[error("GitLab API error: {message}")]
    GitLabError { message: String },
    
    #[error("Stack error: {message}")]
    StackError { message: String },
    
    #[error("Security error: {message}")]
    SecurityError { message: String },
    
    #[error("IO error: {message}")]
    IoError { message: String },
    
    #[error("Serialization error: {message}")]
    SerializationError { message: String },
    
    #[error("Invalid state: {message}")]
    InvalidState { message: String },
} 