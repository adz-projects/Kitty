use thiserror::Error;

#[derive(Error, Debug)]
pub enum DaemonError {
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    #[error("config error: {0}")]
    Config(#[from] ConfigError),

    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),

    #[error("agent error: {0}")]
    Agent(#[from] AgentError),

    #[error("network error: {0}")]
    Network(#[from] NetworkError),

    #[error("mcp error: {0}")]
    Mcp(#[from] MCPServerError),

    #[error("scheduler error: {0}")]
    Scheduler(#[from] SchedulerError),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("crypto init error: {0}")]
    Crypto(String),
}

#[derive(Error, Debug)]
pub enum StorageError {
    #[error("sqlx error: {0}")]
    Sqlx(#[from] sqlx::Error),

    #[error("migration error: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),

    /// A looked-up row does not exist — lets HTTP layers map to 404 by
    /// variant instead of substring-matching the message (which flipped
    /// 404/500 the moment the wording changed).
    #[error("not found: {0}")]
    NotFound(String),

    #[error("storage error: {0}")]
    Generic(String),
}

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("yaml parse error: {0}")]
    Yaml(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Error, Debug)]
pub enum ProviderError {
    #[error("http error: {0}")]
    Http(String),

    #[error("sse parse error: {0}")]
    SseParse(String),

    #[error("not implemented: {0}")]
    NotImplemented(&'static str),

    #[error("classification: {type} — {user_message}")]
    Classification {
        #[allow(dead_code)]
        r#type: &'static str,
        user_message: String,
    },

    #[error("insufficient credits: {user_message}")]
    InsufficientCredits {
        user_message: String,
        raw_message: String,
        http_status: i32,
    },

    #[error("context exceeded: {user_message}")]
    ContextExceeded {
        user_message: String,
        raw_message: String,
        http_status: i32,
    },

    #[error("provider error: {user_message}")]
    Other {
        user_message: String,
        raw_message: String,
        http_status: i32,
    },

    #[error("request failed: {user_message}")]
    Request {
        user_message: String,
        raw_message: String,
        http_status: i32,
    },

    #[error("no healthy provider: {user_message}")]
    NoHealthyProvider { user_message: String },
}

#[derive(Error, Debug)]
pub enum AgentError {
    #[error("context error: {0}")]
    Context(String),

    #[error("compaction error: {0}")]
    Compaction(String),

    #[error("token counting error: {0}")]
    TokenCount(String),

    #[error("agent error: {0}")]
    Generic(String),
}

#[derive(Error, Debug)]
pub enum RecipeError {
    #[error("recipe not found: {0}")]
    NotFound(String),

    #[error("template error: {0}")]
    Template(String),

    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    #[error("turn failed: {0}")]
    TurnFailed(String),
}

#[derive(Error, Debug)]
pub enum SchedulerError {
    #[error("schedule not found: {0}")]
    NotFound(String),

    #[error("cron error: {0}")]
    Cron(String),

    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}

#[derive(Error, Debug)]
pub enum NetworkError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("dns error: {0}")]
    Dns(String),
}

#[derive(Error, Debug)]
pub enum MCPServerError {
    #[error("transport error: {0}")]
    Transport(String),

    #[error("timed out after {0}s")]
    Timeout(f64),

    #[error("mcp error {code}: {message}")]
    Protocol { code: i64, message: String },

    #[error("server not found: {0}")]
    NotFound(String),

    #[error("{0}")]
    Generic(String),
}
