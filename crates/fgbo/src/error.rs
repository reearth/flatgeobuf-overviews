use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("FlatGeobuf error: {0}")]
    Fgb(#[from] flatgeobuf::Error),
    #[error("geozero error: {0}")]
    Geozero(#[from] geozero::error::GeozeroError),
    #[error("invalid FGBO format: {0}")]
    Format(String),
    #[error("not an FGBO file (no footer magic)")]
    NotFgbo,
    #[error("directory CRC mismatch")]
    CrcMismatch,
    #[error("invalid input: {0}")]
    InvalidInput(String),
}

pub type Result<T> = std::result::Result<T, Error>;
