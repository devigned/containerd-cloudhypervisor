use thiserror::Error;

#[derive(Error, Debug)]
pub enum CloudHvError {
    #[error("VM lifecycle error: {0}")]
    VmError(String),

    #[error("Cloud Hypervisor API error: {0}")]
    ApiError(String),

    #[error("Guest agent communication error: {0}")]
    AgentError(String),

    #[error("Container error: {0}")]
    ContainerError(String),

    #[error("Configuration error: {0}")]
    ConfigError(String),

    #[error("Timeout waiting for {0}")]
    Timeout(String),

    #[error("vsock error: {0}")]
    VsockError(String),

    #[error("runc error: exit_code={exit_code}, stderr={stderr}")]
    RuncError { exit_code: i32, stderr: String },

    #[error("mount error: {0}")]
    MountError(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, CloudHvError>;
