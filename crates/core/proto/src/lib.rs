// SPDX-License-Identifier: AGPL-3.0-only

//! What a format's protocol is held to, whichever crate serves it: who
//! may do what ([`gate`]) and how long an answer may be kept
//! ([`cache`]), pure tables, so that the formats that ask them answer
//! alike and a format's own tests can run them (some formats ask the
//! rights rule so far; the cache table is planned, and none asks it
//! yet); the bodies its requests and replies carry ([`body`]), how its
//! query and its upload forms are read ([`query`], [`form`]); what it
//! asks of the server and of its caller ([`host`]).

pub mod body;
pub mod cache;
pub mod form;
pub mod gate;
pub mod host;
pub mod query;

pub use gate::{Gate, Need, Refusal, Role, decide};
pub use host::{Caller, Who};
