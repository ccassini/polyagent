use thiserror::Error;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("network error: {0}")]
    Network(String),
    #[error("execution error: {0}")]
    Execution(String),
    #[error("risk violation: {0}")]
    Risk(String),
    #[error("stale data: {0}")]
    StaleData(String),
    #[error("internal error: {0}")]
    Internal(String),
}

pub type AgentResult<T> = Result<T, AgentError>;
