// Copyright 2024 Adobe. All rights reserved.
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

//! Signature validation info.

use chrono::{DateTime, Utc};
use x509_parser::num_bigint::BigUint;

use crate::crypto::raw_signature::SigningAlg;

/// Summary information about an X.509 signing certificate and the validation
/// performed on it.
#[derive(Debug, Default)]
pub struct CertificateInfo {
    /// Algorithm used to validate the signature
    pub alg: Option<SigningAlg>,

    /// Date the signature was created
    pub date: Option<DateTime<Utc>>,

    /// Certificate serial number
    pub cert_serial_number: Option<BigUint>,

    /// Certificate issuer organization
    pub issuer_org: Option<String>,

    /// Signature validity
    ///
    /// TO REVIEW: What does this `bool` mean?
    pub validated: bool,

    /// Certificate chain used to validate the signature
    pub cert_chain: Vec<u8>,

    /// Signature revocation status
    ///
    /// TO REVIEW: What does this `bool` mean?
    pub revocation_status: Option<bool>,

    /// User attested time (iat) if present
    pub iat: Option<String>,
}
