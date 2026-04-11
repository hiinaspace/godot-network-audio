use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("unsupported configuration: {0}")]
    UnsupportedConfig(&'static str),
    #[error("invalid packet: {0}")]
    InvalidPacket(&'static str),
    #[error("opus error: {0}")]
    Opus(String),
    #[error("neteq error: {0}")]
    NetEq(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<neteq::NetEqError> for Error {
    fn from(value: neteq::NetEqError) -> Self {
        Self::NetEq(value.to_string())
    }
}
