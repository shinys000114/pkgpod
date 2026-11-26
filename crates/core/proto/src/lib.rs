// SPDX-License-Identifier: AGPL-3.0-only

//! What a format's protocol is held to, whichever crate serves it: who
//! may do what ([`gate`]) and how long an answer may be kept
//! ([`cache`]), pure tables, so that every format answers alike and a
//! format's own tests can run them; the bodies its requests and replies
//! carry ([`body`]) and how its query is read ([`query`]).

pub mod body;
pub mod cache;
pub mod gate;
pub mod query;

pub use gate::{Gate, Need, Refusal, Role, decide};
