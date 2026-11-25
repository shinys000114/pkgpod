// SPDX-License-Identifier: AGPL-3.0-only

//! Each format's client protocol, where it differs from plain files: the
//! registries (npm, PyPI, OCI, cargo, …) and their APIs.

pub mod cargo;
pub mod composer;
pub mod conan;
pub mod galaxy;
pub mod gems;
pub mod go;
pub mod helm;
pub mod npm;
pub mod nuget;
pub mod oci;
pub mod pub_api;
pub mod pypi;
pub mod swift;
pub mod terraform;
