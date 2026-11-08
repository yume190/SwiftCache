//! Shared SwiftCache service and desktop management implementation.
// tonic fixes `Result<_, tonic::Status>` (~176 bytes) for every handler and for the generated
// proto code, so boxing the error is not an option.
#![allow(clippy::result_large_err)]
mod blob;
mod hash;
pub mod journal;
pub mod management;
mod peer;
pub mod proto;
mod proxy;
pub mod runtime;
mod service;
mod store;
