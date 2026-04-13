use capsaicin_net::NetError;
use capsaicin_proto::ProtoError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("net: {0}")]
    Net(#[from] NetError),

    #[error("protocol: {0}")]
    Proto(#[from] ProtoError),

    #[error("required channel '{0}' not advertised by server")]
    MissingChannel(&'static str),

    #[error("client has been closed")]
    Closed,
}

pub type Result<T> = std::result::Result<T, ClientError>;
