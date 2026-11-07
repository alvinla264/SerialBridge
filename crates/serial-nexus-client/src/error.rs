use serial_nexus_protocol::{ErrorBody, ErrorCode, ProtocolError};

/// Everything that can go wrong in the client library.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("socket or process error: {0}")]
    Io(#[from] std::io::Error),

    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),

    /// The core answered with an error (details carry its code).
    #[error("core error {:?}: {}", .0.code, .0.message)]
    Server(ErrorBody),

    #[error("request timed out: {0}")]
    Timeout(String),

    /// The connection to the core closed.
    #[error("core connection closed: {0}")]
    Disconnected(String),

    /// No `serial-nexus-core` executable could be found to start.
    #[error(
        "serial-nexus-core was not found (set SERIAL_NEXUS_CORE or ClientConfig::core_binary)"
    )]
    CoreNotFound,

    /// The core executable was found but did not come up.
    #[error("serial-nexus-core did not start: {0}")]
    CoreStart(String),

    #[error("core speaks protocol {core}, this client speaks {client}")]
    UnsupportedProtocol { core: u32, client: u32 },

    #[error("unexpected response from core: {0}")]
    UnexpectedResponse(String),
}

impl ClientError {
    /// The core's error code, when it answered with one.
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Server(body) => Some(body.code),
            _ => None,
        }
    }

    /// True when the error means "the AI does not hold write control".
    pub fn is_control_held(&self) -> bool {
        self.code() == Some(ErrorCode::ControlHeld)
    }

    /// True when the device is gone (unplugged, closed, or failed).
    pub fn is_device_gone(&self) -> bool {
        matches!(
            self.code(),
            Some(ErrorCode::DeviceClosed)
                | Some(ErrorCode::DeviceFailed)
                | Some(ErrorCode::NotFound)
        )
    }
}

impl From<ErrorBody> for ClientError {
    fn from(error: ErrorBody) -> Self {
        if error.code == ErrorCode::Timeout {
            Self::Timeout(error.message)
        } else {
            Self::Server(error)
        }
    }
}
