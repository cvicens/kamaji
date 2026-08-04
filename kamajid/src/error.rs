use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum AttachmentError {
    #[error("timed out talking to telegram for file metadata/download")]
    Timeout,
    #[error("failed to resolve telegram file: {0}")]
    GetFile(#[source] teloxide::RequestError),
    #[error("failed to download telegram file: {0}")]
    Download(#[source] teloxide::DownloadError),
    #[error("failed to download matrix media: {0}")]
    MatrixMedia(#[source] matrix_sdk::Error),
    #[error("attachment is {size} bytes, over the {max}-byte limit")]
    TooLarge { size: usize, max: usize },
    #[error("attachment's platform client is not configured")]
    ClientNotConfigured,
}

#[derive(Debug, thiserror::Error)]
pub enum MatrixClientError {
    #[error("failed to create matrix store directory {path}: {source}")]
    CreateStoreDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to build matrix client: {0}")]
    Build(#[source] Box<matrix_sdk::ClientBuildError>),
    #[error("invalid matrix user id: {0}")]
    ParseUserId(#[source] matrix_sdk::ruma::IdParseError),
    #[error("failed to restore matrix session: {0}")]
    RestoreSession(#[source] matrix_sdk::Error),
    #[error("failed to serialize matrix session: {0}")]
    Serialize(#[source] serde_json::Error),
    #[error("failed to write matrix session file {path}: {source}")]
    WriteSessionFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum SocketError {
    #[error("failed to bind unix socket at {path}: {source}")]
    Bind {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("another kamajid instance is already listening on {0}")]
    AlreadyRunning(PathBuf),
    #[error("failed to remove stale socket file {path}: {source}")]
    RemoveStale {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}
