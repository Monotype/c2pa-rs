// Copyright 2025 Adobe. All rights reserved.
// This file is licensed to you under the Apache License,
// Version 2.0 (http://www.apache.org/licenses/LICENSE-2.0)
// or the MIT license (http://opensource.org/licenses/MIT),
// at your option.

// Unless required by applicable law or agreed to in writing,
// this software is distributed on an "AS IS" BASIS, WITHOUT
// WARRANTIES OR REPRESENTATIONS OF ANY KIND, either express or
// implied. See the LICENSE-MIT and LICENSE-APACHE files for the
// specific language governing permissions and limitations under
// each license.

#![allow(unused)] // Not used on all platforms or all configs

//! Experimental support for synchronous raw signatures using Rust-native
//! crates. Intended mostly for the synchronous use cases on WASM, but may
//! eventually be used on all platforms.
//!
//! At the moment, does not offer support for all C2PA-supported cryptography
//! algorithms.

pub(crate) mod signers;
pub(crate) mod validators;