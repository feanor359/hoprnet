use hopr_primitive_types::errors::GeneralError;
use k256::elliptic_curve;
use thiserror::Error;

#[derive(Error, Debug, PartialEq)]
pub enum CryptoError {
    #[error("cryptographic parameter '{name}' must be {expected} bytes long")]
    InvalidParameterSize { name: &'static str, expected: usize },

    #[error("input to the function '{0}' has invalid value or size")]
    InvalidInputValue(&'static str),

    #[error("secret scalar results in an invalid EC point")]
    InvalidSecretScalar,

    #[error("ec point represents and invalid public key")]
    InvalidPublicKey,

    #[error("mac or authentication tag did not match")]
    TagMismatch,

    #[error("curve error: {0}")]
    EllipticCurveError(#[from] elliptic_curve::Error),

    #[error("failed to perform cryptographic calculation")]
    CalculationError,

    #[error("signature verification failed")]
    SignatureVerification,

    #[error("error during sealing/unsealing of data")]
    SealingError,

    #[error("ethereum challenge on the ticket is invalid")]
    InvalidChallenge,

    #[error("invalid vrf values")]
    InvalidVrfValues,

    #[error("lower-level error: {0}")]
    Other(#[from] GeneralError),
}

pub type Result<T> = core::result::Result<T, CryptoError>;
