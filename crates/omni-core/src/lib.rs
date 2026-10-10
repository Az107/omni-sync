//! Pure sync semantics for omni-sync.
//!
//! This crate owns the rules that decide what a change *means* — the shape of
//! an op, the shape of a version, and the sequencer rule that turns a submitted
//! version plus the active one into a decision. It has no I/O, no database and
//! no network on purpose: the server's sequencer and the client's applier both
//! call into here, so a rule change is one edit and one test, not two
//! implementations that drift.
//!
//! A path is a [`PathBytes`] (`Vec<u8>`), never a `String`. Linux filenames are
//! not guaranteed to be UTF-8, and a lossy round-trip is how one device's
//! `résumé.pdf` becomes another device's differently-named file.

mod op;
mod sequencer;
mod version;

pub use op::{ChunkRef, Op, OpKind};
pub use sequencer::{Decision, Sequencer};
pub use version::{PathBytes, Version};
