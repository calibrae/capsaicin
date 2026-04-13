use crate::{ProtoError, Result};

pub const SPICE_MAGIC: u32 = 0x5144_4552; // "REDQ" in little-endian
pub const SPICE_VERSION_MAJOR: u32 = 2;
pub const SPICE_VERSION_MINOR: u32 = 2;

pub const SPICE_TICKET_PUBKEY_BYTES: usize = 162;
pub const SPICE_TICKET_KEY_PAIR_LENGTH: usize = 1024;
pub const SPICE_MAX_PASSWORD_LENGTH: usize = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ChannelType {
    Main = 1,
    Display = 2,
    Inputs = 3,
    Cursor = 4,
    Playback = 5,
    Record = 6,
    Tunnel = 7,
    Smartcard = 8,
    UsbRedir = 9,
    Port = 10,
    WebDav = 11,
}

impl ChannelType {
    pub fn from_u8(v: u8) -> Result<Self> {
        Ok(match v {
            1 => Self::Main,
            2 => Self::Display,
            3 => Self::Inputs,
            4 => Self::Cursor,
            5 => Self::Playback,
            6 => Self::Record,
            7 => Self::Tunnel,
            8 => Self::Smartcard,
            9 => Self::UsbRedir,
            10 => Self::Port,
            11 => Self::WebDav,
            _ => return Err(ProtoError::BadChannelType(v)),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum LinkError {
    Ok = 0,
    Error = 1,
    InvalidMagic = 2,
    InvalidData = 3,
    VersionMismatch = 4,
    NeedSecured = 5,
    NeedUnsecured = 6,
    PermissionDenied = 7,
    BadConnectionId = 8,
    ChannelNotAvailable = 9,
}

impl LinkError {
    pub fn from_u32(v: u32) -> Result<Self> {
        Ok(match v {
            0 => Self::Ok,
            1 => Self::Error,
            2 => Self::InvalidMagic,
            3 => Self::InvalidData,
            4 => Self::VersionMismatch,
            5 => Self::NeedSecured,
            6 => Self::NeedUnsecured,
            7 => Self::PermissionDenied,
            8 => Self::BadConnectionId,
            9 => Self::ChannelNotAvailable,
            _ => return Err(ProtoError::BadLinkError(v)),
        })
    }
}

/// Common message types sent by the server (every channel).
pub mod msg {
    pub const MIGRATE: u16 = 1;
    pub const MIGRATE_DATA: u16 = 2;
    pub const SET_ACK: u16 = 3;
    pub const PING: u16 = 4;
    pub const WAIT_FOR_CHANNELS: u16 = 5;
    pub const DISCONNECTING: u16 = 6;
    pub const NOTIFY: u16 = 7;
    pub const LIST: u16 = 8;
}

/// Common message types sent by the client (every channel).
pub mod msgc {
    pub const ACK_SYNC: u16 = 1;
    pub const ACK: u16 = 2;
    pub const PONG: u16 = 3;
    pub const MIGRATE_FLUSH_MARK: u16 = 4;
    pub const MIGRATE_DATA: u16 = 5;
    pub const DISCONNECTING: u16 = 6;
}

/// Main channel server messages (base 101).
pub mod main_msg {
    pub const MIGRATE_BEGIN: u16 = 101;
    pub const MIGRATE_CANCEL: u16 = 102;
    pub const INIT: u16 = 103;
    pub const CHANNELS_LIST: u16 = 104;
    pub const MOUSE_MODE: u16 = 105;
    pub const MULTI_MEDIA_TIME: u16 = 106;
    pub const AGENT_CONNECTED: u16 = 107;
    pub const AGENT_DISCONNECTED: u16 = 108;
    pub const AGENT_DATA: u16 = 109;
    pub const AGENT_TOKEN: u16 = 110;
    pub const MIGRATE_SWITCH_HOST: u16 = 111;
    pub const MIGRATE_END: u16 = 112;
    pub const NAME: u16 = 113;
    pub const UUID: u16 = 114;
    pub const AGENT_CONNECTED_TOKENS: u16 = 115;
    pub const MIGRATE_BEGIN_SEAMLESS: u16 = 116;
    pub const MIGRATE_DST_SEAMLESS_ACK: u16 = 117;
    pub const MIGRATE_DST_SEAMLESS_NACK: u16 = 118;
}

/// Main channel client messages (base 101).
pub mod main_msgc {
    pub const CLIENT_INFO: u16 = 101;
    pub const MIGRATE_CONNECTED: u16 = 102;
    pub const MIGRATE_CONNECT_ERROR: u16 = 103;
    pub const ATTACH_CHANNELS: u16 = 104;
    pub const MOUSE_MODE_REQUEST: u16 = 105;
    pub const AGENT_START: u16 = 106;
    pub const AGENT_DATA: u16 = 107;
    pub const AGENT_TOKEN: u16 = 108;
    pub const MIGRATE_END: u16 = 109;
    pub const MIGRATE_DST_DO_SEAMLESS: u16 = 110;
    pub const MIGRATE_CONNECTED_SEAMLESS: u16 = 111;
}
