// SPDX-License-Identifier: AGPL-3.0-only

//! Running the server: its listeners, recent log, metrics, outgoing
//! webhooks, the CLI it hands out, and Markdown notes.

pub mod cli_dl;
pub mod listeners;
pub mod logs;
pub mod metrics;
pub mod notes;
pub mod webhooks;
