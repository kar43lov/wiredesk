use thiserror::Error;

#[derive(Error, Debug)]
pub enum WireDeskError {
    #[error("transport: {0}")]
    Transport(String),

    #[error("protocol: {0}")]
    Protocol(String),

    #[error("input: {0}")]
    Input(String),

    #[error("clipboard: {0}")]
    Clipboard(String),

    #[error("config: {0}")]
    Config(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, WireDeskError>;
