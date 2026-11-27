// SPDX-License-Identifier: AGPL-3.0-only

//! Galaxy's API is pkgpod-galaxy's ([`pkgpod_galaxy::protocol`]): the
//! API root, collections, their versions and downloads, and publishing,
//! for ansible-galaxy; the App is its Host as it is every protocol's.

pub use pkgpod_galaxy::protocol::{Ask, handle};
