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

//! This module binds Rust native logic for generating raw signatures to this
//! crate's [`RawSignatureValidator`] trait.

use bcder::Oid;

use crate::raw_signature::{oids::*, RawSignatureValidator, SigningAlg};

mod ed25519_validator;
pub use ed25519_validator::Ed25519Validator;

/// Return a validator for the given signing algorithm.
pub fn validator_for_signing_alg(alg: SigningAlg) -> Option<Box<dyn RawSignatureValidator>> {
    match alg {
        SigningAlg::Ed25519 => Some(Box::new(Ed25519Validator {})),
        _ => None,
    }
}