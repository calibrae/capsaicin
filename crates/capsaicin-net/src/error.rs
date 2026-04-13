use capsaicin_proto::{ProtoError, enums::LinkError};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum NetError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("protocol: {0}")]
    Proto(#[from] ProtoError),

    #[error("peer reported link error: {0:?}")]
    Link(LinkError),

    #[error("peer sent message larger than {max} bytes (got {size})")]
    MessageTooLarge { size: u32, max: u32 },

    #[error("server public key is invalid")]
    BadServerKey,

    #[error("password is too long ({len} > 60 bytes)")]
    PasswordTooLong { len: usize },

    #[error("rsa encrypt: {0}")]
    RsaEncrypt(String),
}

pub type Result<T> = std::result::Result<T, NetError>;
