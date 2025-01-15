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

use async_trait::async_trait;

use crate::{SignerPayload, ValidationError};

/// A `Verifier` can read one or more kinds of signature from an identity
/// assertion, assess the validity of the signature, and return information
/// about the corresponding credential subject.
///
/// The associated type `Output` describes the information which can be derived
/// from the credential and signature.
#[cfg(not(target_arch = "wasm32"))]
#[async_trait]
pub trait SignatureVerifier: Sync {
    /// The `Output` type provides credential-type specific information that is
    /// derived from the signature. Typically, this describes the named actor,
    /// but may also contain information about the time of signing or the
    /// credential's source.
    type Output;

    /// The `Error` type provides a credential-specific explanation for why an
    /// identity assertion signature could not be accepted. This value may be
    /// included in the `SignatureError` variant of [`ValidationError`].
    ///
    /// [`ValidationError`]: crate::ValidationError
    type Error;

    /// Verify the signature, returning an instance of [`Output`] if the
    /// signature is valid.
    ///
    /// [`Output`]: Self::Output
    async fn check_signature(
        &self,
        signer_payload: &SignerPayload,
        signature: &[u8],
    ) -> Result<Self::Output, ValidationError<Self::Error>>;
}

/// A `Verifier` can read one or more kinds of signature from an identity
/// assertion, assess the validity of the signature, and return information
/// about the corresponding credential subject.
///
/// The associated type `Output` describes the information which can be derived
/// from the credential and signature.
#[cfg(target_arch = "wasm32")]
#[async_trait(?Send)]
pub trait SignatureVerifier {
    /// The `Output` type provides credential-type specific information that is
    /// derived from the signature. Typically, this describes the named actor,
    /// but may also contain information about the time of signing or the
    /// credential's source.
    type Output;

    /// The `Error` type provides a credential-specific explanation for why an
    /// identity assertion signature could not be accepted. This value may be
    /// included in the `SignatureError` variant of [`ValidationError`].
    ///
    /// [`ValidationError`]: crate::ValidationError
    type Error;

    /// Verify the signature, returning an instance of [`Output`] if the
    /// signature is valid.
    ///
    /// [`Output`]: Self::Output
    async fn check_signature(
        &self,
        signer_payload: &SignerPayload,
        signature: &[u8],
    ) -> Result<Self::Output, ValidationError<Self::Error>>;
}