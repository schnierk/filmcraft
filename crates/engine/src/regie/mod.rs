//! KI-Regie (AI Director): make a film from a sentence.
//!
//! A beginner says what they want ("a 16:9 4K recap of this year's holidays, upbeat, about three
//! minutes") and FilmCraft plans and builds a normal, fully editable sequence from the user's own
//! media. The work is split into steps that are commands of their own, so the panel, the CLI, the
//! control channel and MCP agents can run all of them at once (`regie.run`) or one by one:
//!
//! | Step | Commands | Module |
//! |---|---|---|
//! | Understand the request | `regie.plan` | [`plan`] |
//! | Find the media (capture date, place, orientation) | `media.captureInfo`, `regie.findMedia` | [`library`] |
//! | Best moments, music beats | `media.highlights`, `audio.beats` | [`analysis`] |
//! | Build the sequence (one undo step) | `regie.assemble`, `regie.run`, `regie.status` | [`assemble`] |
//!
//! The data passed between the steps is in [`types`] and documented in `docs/regie.md`. Everything
//! runs locally; a language model is optional and only refines the plan.

pub mod analysis;
pub mod assemble;
pub mod library;
pub mod plan;
pub mod run;
#[cfg(test)]
mod tests;
pub mod types;

pub use types::{Beats, Candidate, Highlights, Plan, Section, Segment};

use crate::commands::CommandSpec;

pub(crate) fn commands() -> Vec<CommandSpec> {
    let mut v = plan::commands();
    v.extend(library::commands());
    v.extend(analysis::commands());
    v.extend(assemble::commands());
    v
}
