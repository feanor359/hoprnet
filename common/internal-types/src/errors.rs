use hopr_crypto_types::errors::CryptoError;
use multiaddr::Error as MultiaddrError;
use thiserror::Error;

/// Enumeration of all core type related errors.
#[derive(Error, Debug)]
pub enum CoreTypesError {
    #[error("{0}")]
    InvalidInputData(String),

    #[error("failed to parse/deserialize the data of {0}")]
    ParseError(String),

    #[error("Arithmetic error: {0}")]
    ArithmeticError(String),

    #[error("Invalid ticket signature or wrong ticket recipient")]
    InvalidTicketRecipient,

    #[error("Cannot acknowledge self-signed tickets. Ticket sender and recipient must be different")]
    LoopbackTicket,

    #[error("size of the packet payload has been exceeded")]
    PayloadSizeExceeded,

    #[error(transparent)]
    InvalidMultiaddr(#[from] MultiaddrError),

    #[error(transparent)]
    CryptoError(#[from] CryptoError),

    #[error(transparent)]
    GeneralError(#[from] hopr_primitive_types::errors::GeneralError),
}

pub type Result<T> = core::result::Result<T, CoreTypesError>;
