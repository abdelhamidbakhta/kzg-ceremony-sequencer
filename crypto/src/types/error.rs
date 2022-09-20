use crate::zcash_format::ParseError;
use thiserror::Error;

#[derive(Clone, Copy, PartialEq, Debug, Error)]
#[allow(clippy::module_name_repetitions)]
pub enum CeremoniesError {
    #[error("Error in contribution {0}: {1}")]
    InvalidCeremony(usize, #[source] CeremonyError),
    #[error("Unexpected number of contributions: expected {0}, got {1}")]
    InvalidCeremoniesCount(usize, usize),
}

#[derive(Clone, Copy, PartialEq, Debug, Error)]
#[allow(clippy::module_name_repetitions)]
pub enum CeremonyError {
    #[error("Unexpected number of G1 powers: expected {0}, got {1}")]
    UnexpectedNumG1Powers(usize, usize),
    #[error("Unexpected number of G2 powers: expected {0}, got {1}")]
    UnexpectedNumG2Powers(usize, usize),
    #[error("Inconsistent number of G1 powers: numG1Powers = {0}, len = {1}")]
    InconsistentNumG1Powers(usize, usize),
    #[error("Inconsistent number of G2 powers: numG2Powers = {0}, len = {1}")]
    InconsistentNumG2Powers(usize, usize),
    #[error("Error parsing G1 power {0}: {1}")]
    InvalidG1Power(usize, #[source] ParseError),
    #[error("Error parsing G2 power {0}: {1}")]
    InvalidG2Power(usize, #[source] ParseError),
    #[error("Error parsing potPubkey: {0}")]
    InvalidPubKey(#[source] ParseError),
    #[error("Error parsing running product {0}: {1}")]
    InvalidWitnessProduct(usize, #[source] ParseError),
    #[error("Error parsing potPubkey {0}: {1}")]
    InvalidWitnessPubKey(usize, #[source] ParseError),
}
