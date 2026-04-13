use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProtoError {
    #[error("buffer too short: need {need} bytes, have {have}")]
    Short { need: usize, have: usize },

    #[error("invalid magic: expected {expected:#x}, got {got:#x}")]
    BadMagic { expected: u32, got: u32 },

    #[error("unsupported version {major}.{minor}")]
    BadVersion { major: u32, minor: u32 },

    #[error("invalid channel type {0}")]
    BadChannelType(u8),

    #[error("invalid link error code {0}")]
    BadLinkError(u32),

    #[error("caps offset {offset} out of range (buffer len {len})")]
    BadCapsOffset { offset: u32, len: usize },

    #[error("declared size {declared} exceeds max {max}")]
    SizeTooLarge { declared: u32, max: u32 },
}

pub type Result<T> = std::result::Result<T, ProtoError>;
