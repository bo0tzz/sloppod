//! Extension-point contracts (ADR 7).
//!
//! Every contract is a trait whose inputs and outputs are plain serde types, so an
//! implementation can later move behind a process boundary without changing callers.

use serde::{Deserialize, Serialize};

/// Describes an implementation of one or more extension points.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    pub version: String,
}
