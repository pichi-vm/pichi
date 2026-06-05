//! `PMI` loader for dillo.
//!
//! Parses a `PMI` file (PE + per-target CBOR manifest in `.pmi.<target>`),
//! enforces dillo's defensive resource caps, validates spec-mandated and
//! beyond-spec rules, and produces a [`ParsedPmi`] describing what to
//! load and where.
//!
//! See `dillo/ARCHITECTURE.md` §5 for the full contract.

pub mod caps;
pub mod error;
mod parse;

pub use error::Error;
pub use parse::{
    Action, FillKind, HostArch, ParseOptions, ParsedPmi, SectionInfo, VcpuState, parse,
};
pub use pmi::cpu::Profile;
