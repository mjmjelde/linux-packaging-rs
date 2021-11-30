// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

/*! Debian packaging primitives.

This crate defines pure Rust implementations of Debian packaging primitives.
*/

pub mod binary_package_control;
pub mod binary_package_list;
pub mod changelog;
pub mod control;
pub mod deb;
pub mod dependency;
pub mod dependency_resolution;
pub mod error;
pub mod package_version;
pub mod pgp;
pub mod repository;
