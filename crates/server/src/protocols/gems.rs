// SPDX-License-Identifier: AGPL-3.0-only

//! The RubyGems API is pkgpod-gems' ([`pkgpod_gems::protocol`]): `gem
//! push` and `gem yank`, beside the tree the server serves (the compact
//! index, gems, quick gemspecs).

pub use pkgpod_gems::protocol::{Ask, handle, is_api};
