// Copyright 2022 Adobe. All rights reserved.
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

use std::io::Cursor;

use asn1_rs::{Any, Class, Header, Tag};
use async_generic::async_generic;
use c2pa_crypto::{
    asn1::rfc3161::TstInfo,
    cose::{
        parse_and_validate_sigtst, parse_and_validate_sigtst_async, CertificateAcceptancePolicy,
    },
    ocsp::OcspResponse,
    p1363::parse_ec_der_sig,
    raw_signature::{validator_for_signing_alg, RawSignatureValidator},
    time_stamp::TimeStampError,
    SigningAlg, ValidationInfo,
};
use c2pa_status_tracker::{log_item, validation_codes::*, StatusTracker};
use ciborium::value::Value;
use conv::*;
use coset::{
    iana::{self, EnumI64},
    sig_structure_data, Label, TaggedCborSerializable,
};
use x509_parser::{
    der_parser::{ber::parse_ber_sequence, oid},
    num_bigint::BigUint,
    oid_registry::Oid,
    prelude::*,
};

#[cfg(feature = "openssl")]
use crate::openssl::verify_trust;
#[cfg(target_arch = "wasm32")]
use crate::wasm::webpki_trust_handler::verify_trust_async;
use crate::{
    error::{Error, Result},
    settings::get_settings_value,
};

pub(crate) const RSA_OID: Oid<'static> = oid!(1.2.840 .113549 .1 .1 .1);
pub(crate) const EC_PUBLICKEY_OID: Oid<'static> = oid!(1.2.840 .10045 .2 .1);
pub(crate) const RSASSA_PSS_OID: Oid<'static> = oid!(1.2.840 .113549 .1 .1 .10);

pub(crate) const ECDSA_WITH_SHA256_OID: Oid<'static> = oid!(1.2.840 .10045 .4 .3 .2);
pub(crate) const ECDSA_WITH_SHA384_OID: Oid<'static> = oid!(1.2.840 .10045 .4 .3 .3);
pub(crate) const ECDSA_WITH_SHA512_OID: Oid<'static> = oid!(1.2.840 .10045 .4 .3 .4);
pub(crate) const SHA256_WITH_RSAENCRYPTION_OID: Oid<'static> = oid!(1.2.840 .113549 .1 .1 .11);
pub(crate) const SHA384_WITH_RSAENCRYPTION_OID: Oid<'static> = oid!(1.2.840 .113549 .1 .1 .12);
pub(crate) const SHA512_WITH_RSAENCRYPTION_OID: Oid<'static> = oid!(1.2.840 .113549 .1 .1 .13);
pub(crate) const ED25519_OID: Oid<'static> = oid!(1.3.101 .112);
#[allow(dead_code)] // used only in WASM build
pub(crate) const SHA1_OID: Oid<'static> = oid!(1.3.14 .3 .2 .26);
pub(crate) const SHA256_OID: Oid<'static> = oid!(2.16.840 .1 .101 .3 .4 .2 .1);
pub(crate) const SHA384_OID: Oid<'static> = oid!(2.16.840 .1 .101 .3 .4 .2 .2);
pub(crate) const SHA512_OID: Oid<'static> = oid!(2.16.840 .1 .101 .3 .4 .2 .3);
pub(crate) const SECP521R1_OID: Oid<'static> = oid!(1.3.132 .0 .35);
pub(crate) const SECP384R1_OID: Oid<'static> = oid!(1.3.132 .0 .34);
pub(crate) const PRIME256V1_OID: Oid<'static> = oid!(1.2.840 .10045 .3 .1 .7);

/********************** Supported Validators ***************************************
    RS256	RSASSA-PKCS1-v1_5 using SHA-256 - not recommended
    RS384	RSASSA-PKCS1-v1_5 using SHA-384 - not recommended
    RS512	RSASSA-PKCS1-v1_5 using SHA-512 - not recommended
    PS256	RSASSA-PSS using SHA-256 and MGF1 with SHA-256
    PS384	RSASSA-PSS using SHA-384 and MGF1 with SHA-384
    PS512	RSASSA-PSS using SHA-512 and MGF1 with SHA-512
    ES256	ECDSA using P-256 and SHA-256
    ES384	ECDSA using P-384 and SHA-384
    ES512	ECDSA using P-521 and SHA-512
    ED25519 Edwards Curve 25519
**********************************************************************************/

fn get_cose_sign1(
    cose_bytes: &[u8],
    data: &[u8],
    validation_log: &mut impl StatusTracker,
) -> Result<coset::CoseSign1> {
    match <coset::CoseSign1 as TaggedCborSerializable>::from_tagged_slice(cose_bytes) {
        Ok(mut sign1) => {
            sign1.payload = Some(data.to_vec()); // restore payload for verification check
            Ok(sign1)
        }
        Err(coset_error) => {
            log_item!(
                "Cose_Sign1",
                "could not deserialize signature",
                "get_cose_sign1"
            )
            .validation_status(CLAIM_SIGNATURE_MISMATCH)
            .failure_no_throw(validation_log, Error::InvalidCoseSignature { coset_error });

            Err(Error::CoseSignature)
        }
    }
}

pub(crate) fn check_cert(
    ca_der_bytes: &[u8],
    cap: &CertificateAcceptancePolicy,
    validation_log: &mut impl StatusTracker,
    _tst_info_opt: Option<&TstInfo>,
) -> Result<()> {
    // get the cert in der format
    let (_rem, signcert) = X509Certificate::from_der(ca_der_bytes).map_err(|_err| {
        log_item!(
            "Cose_Sign1",
            "certificate could not be parsed",
            "check_cert_alg"
        )
        .validation_status(SIGNING_CREDENTIAL_INVALID)
        .failure_no_throw(validation_log, Error::CoseInvalidCert);

        Error::CoseInvalidCert
    })?;

    // cert version must be 3
    if signcert.version() != X509Version::V3 {
        log_item!(
            "Cose_Sign1",
            "certificate version incorrect",
            "check_cert_alg"
        )
        .validation_status(SIGNING_CREDENTIAL_INVALID)
        .failure_no_throw(validation_log, Error::CoseInvalidCert);

        return Err(Error::CoseInvalidCert);
    }

    // check for cert expiration
    if let Some(tst_info) = _tst_info_opt {
        // was there a time stamp association with this signature, is verify against that time
        let signing_time = gt_to_datetime(tst_info.gen_time.clone());
        if !signcert.validity().is_valid_at(
            x509_parser::time::ASN1Time::from_timestamp(signing_time.timestamp())
                .map_err(|_| Error::CoseInvalidCert)?,
        ) {
            log_item!("Cose_Sign1", "certificate expired", "check_cert_alg")
                .validation_status(SIGNING_CREDENTIAL_EXPIRED)
                .failure_no_throw(validation_log, Error::CoseCertExpiration);

            return Err(Error::CoseCertExpiration);
        }
    } else {
        // no timestamp so check against current time
        // use instant to avoid wasm issues
        let now_f64 = instant::now() / 1000.0;
        let now: i64 = now_f64
            .approx_as::<i64>()
            .map_err(|_e| Error::BadParam("system time invalid".to_string()))?;

        if !signcert.validity().is_valid_at(
            x509_parser::time::ASN1Time::from_timestamp(now).map_err(|_| Error::CoseInvalidCert)?,
        ) {
            log_item!("Cose_Sign1", "certificate expired", "check_cert_alg")
                .validation_status(SIGNING_CREDENTIAL_EXPIRED)
                .failure_no_throw(validation_log, Error::CoseCertExpiration);

            return Err(Error::CoseCertExpiration);
        }
    }

    let cert_alg = signcert.signature_algorithm.algorithm.clone();

    // check algorithm needed from cert

    // cert must be signed with one the following algorithm
    if !(cert_alg == SHA256_WITH_RSAENCRYPTION_OID
        || cert_alg == SHA384_WITH_RSAENCRYPTION_OID
        || cert_alg == SHA512_WITH_RSAENCRYPTION_OID
        || cert_alg == ECDSA_WITH_SHA256_OID
        || cert_alg == ECDSA_WITH_SHA384_OID
        || cert_alg == ECDSA_WITH_SHA512_OID
        || cert_alg == RSASSA_PSS_OID
        || cert_alg == ED25519_OID)
    {
        log_item!(
            "Cose_Sign1",
            "certificate algorithm not supported",
            "check_cert_alg"
        )
        .validation_status(SIGNING_CREDENTIAL_INVALID)
        .failure_no_throw(validation_log, Error::CoseInvalidCert);

        return Err(Error::CoseInvalidCert);
    }

    // verify rsassa_pss parameters
    if cert_alg == RSASSA_PSS_OID {
        if let Some(parameters) = &signcert.signature_algorithm.parameters {
            let seq = parameters
                .as_sequence()
                .map_err(|_err| Error::CoseInvalidCert)?;

            let (_i, (ha_alg, mgf_ai)) = seq
                .parse(|i| {
                    let (i, h) = <Header as asn1_rs::FromDer>::from_der(i)?;
                    if h.class() != Class::ContextSpecific || h.tag() != Tag(0) {
                        return Err(nom::Err::Error(asn1_rs::Error::BerValueError));
                    }

                    let (i, ha_alg) = AlgorithmIdentifier::from_der(i)
                        .map_err(|_| nom::Err::Error(asn1_rs::Error::BerValueError))?;

                    let (i, h) = <Header as asn1_rs::FromDer>::from_der(i)?;
                    if h.class() != Class::ContextSpecific || h.tag() != Tag(1) {
                        return Err(nom::Err::Error(asn1_rs::Error::BerValueError));
                    }

                    let (i, mgf_ai) = AlgorithmIdentifier::from_der(i)
                        .map_err(|_| nom::Err::Error(asn1_rs::Error::BerValueError))?;

                    // Ignore anything that follows these two parameters.

                    Ok((i, (ha_alg, mgf_ai)))
                })
                .map_err(|_| Error::CoseInvalidCert)?;

            let mgf_ai_parameters = mgf_ai.parameters.ok_or(Error::CoseInvalidCert)?;
            let mgf_ai_parameters = mgf_ai_parameters
                .as_sequence()
                .map_err(|_| Error::CoseInvalidCert)?;

            let (_i, mgf_ai_params_algorithm) =
                <Any as asn1_rs::FromDer>::from_der(&mgf_ai_parameters.content)
                    .map_err(|_| Error::CoseInvalidCert)?;

            let mgf_ai_params_algorithm = mgf_ai_params_algorithm
                .as_oid()
                .map_err(|_| Error::CoseInvalidCert)?;

            // must be the same
            if ha_alg.algorithm.to_id_string() != mgf_ai_params_algorithm.to_id_string() {
                log_item!(
                    "Cose_Sign1",
                    "certificate algorithm error",
                    "check_cert_alg"
                )
                .validation_status(SIGNING_CREDENTIAL_INVALID)
                .failure_no_throw(validation_log, Error::CoseInvalidCert);

                return Err(Error::CoseInvalidCert);
            }

            // check for one of the mandatory types
            if !(ha_alg.algorithm == SHA256_OID
                || ha_alg.algorithm == SHA384_OID
                || ha_alg.algorithm == SHA512_OID)
            {
                log_item!(
                    "Cose_Sign1",
                    "certificate hash algorithm not supported",
                    "check_cert_alg"
                )
                .validation_status(SIGNING_CREDENTIAL_INVALID)
                .failure_no_throw(validation_log, Error::CoseInvalidCert);

                return Err(Error::CoseInvalidCert);
            }
        } else {
            log_item!(
                "Cose_Sign1",
                "certificate missing algorithm parameters",
                "check_cert_alg"
            )
            .validation_status(SIGNING_CREDENTIAL_INVALID)
            .failure_no_throw(validation_log, Error::CoseInvalidCert);

            return Err(Error::CoseInvalidCert);
        }
    }

    // check curves for SPKI EC algorithms
    let pk = signcert.public_key();
    let skpi_alg = &pk.algorithm;

    if skpi_alg.algorithm == EC_PUBLICKEY_OID {
        if let Some(parameters) = &skpi_alg.parameters {
            let named_curve_oid = parameters.as_oid().map_err(|_err| Error::CoseInvalidCert)?;

            // must be one of these named curves
            if !(named_curve_oid == PRIME256V1_OID
                || named_curve_oid == SECP384R1_OID
                || named_curve_oid == SECP521R1_OID)
            {
                log_item!(
                    "Cose_Sign1",
                    "certificate unsupported EC curve",
                    "check_cert_alg"
                )
                .validation_status(SIGNING_CREDENTIAL_INVALID)
                .failure_no_throw(validation_log, Error::CoseInvalidCert);

                return Err(Error::CoseInvalidCert);
            }
        } else {
            return Err(Error::CoseInvalidCert);
        }
    }

    // check modulus minimum length (for RSA & PSS algorithms)
    if skpi_alg.algorithm == RSA_OID || skpi_alg.algorithm == RSASSA_PSS_OID {
        let (_, skpi_ber) = parse_ber_sequence(&pk.subject_public_key.data)
            .map_err(|_err| Error::CoseInvalidCert)?;

        let seq = skpi_ber
            .as_sequence()
            .map_err(|_err| Error::CoseInvalidCert)?;
        if seq.len() < 2 {
            return Err(Error::CoseInvalidCert);
        }

        let modulus = seq[0].as_bigint().map_err(|_| Error::CoseInvalidCert)?;

        if modulus.bits() < 2048 {
            log_item!(
                "Cose_Sign1",
                "certificate key length too short",
                "check_cert_alg"
            )
            .validation_status(SIGNING_CREDENTIAL_INVALID)
            .failure_no_throw(validation_log, Error::CoseInvalidCert);

            return Err(Error::CoseInvalidCert);
        }
    }

    // check cert values
    let tbscert = &signcert.tbs_certificate;

    let is_self_signed = tbscert.is_ca() && tbscert.issuer() == tbscert.subject();

    // self signed certs are disallowed
    if is_self_signed {
        log_item!(
            "Cose_Sign1",
            "certificate issuer and subject cannot be the same {self-signed disallowed}",
            "check_cert_alg"
        )
        .validation_status(SIGNING_CREDENTIAL_INVALID)
        .failure_no_throw(validation_log, Error::CoseInvalidCert);

        return Err(Error::CoseInvalidCert);
    }

    // unique ids are not allowed
    if signcert.issuer_uid.is_some() || signcert.subject_uid.is_some() {
        log_item!(
            "Cose_Sign1",
            "certificate issuer/subject unique ids are not allowed",
            "check_cert_alg"
        )
        .validation_status(SIGNING_CREDENTIAL_INVALID)
        .failure_no_throw(validation_log, Error::CoseInvalidCert);

        return Err(Error::CoseInvalidCert);
    }

    let mut aki_good = false;
    let mut ski_good = false;
    let mut key_usage_good = false;
    let mut handled_all_critical = true;
    let extended_key_usage_good = match tbscert
        .extended_key_usage()
        .map_err(|_| Error::CoseInvalidCert)?
    {
        Some(BasicExtension { value: eku, .. }) => {
            if eku.any {
                log_item!(
                    "Cose_Sign1",
                    "certificate 'any' EKU not allowed",
                    "check_cert_alg"
                )
                .validation_status(SIGNING_CREDENTIAL_INVALID)
                .failure_no_throw(validation_log, Error::CoseInvalidCert);

                return Err(Error::CoseInvalidCert);
            }

            if cap.has_allowed_eku(eku).is_none() {
                log_item!(
                    "Cose_Sign1",
                    "certificate missing required EKU",
                    "check_cert_alg"
                )
                .validation_status(SIGNING_CREDENTIAL_INVALID)
                .failure_no_throw(validation_log, Error::CoseInvalidCert);

                return Err(Error::CoseInvalidCert);
            }

            // one or the other || either of these two, and no others field
            if (eku.ocsp_signing && eku.time_stamping)
                || ((eku.ocsp_signing ^ eku.time_stamping)
                    && (eku.client_auth
                        | eku.code_signing
                        | eku.email_protection
                        | eku.server_auth
                        | !eku.other.is_empty()))
            {
                log_item!(
                    "Cose_Sign1",
                    "certificate invalid set of EKUs",
                    "check_cert_alg"
                )
                .validation_status(SIGNING_CREDENTIAL_INVALID)
                .failure_no_throw(validation_log, Error::CoseInvalidCert);

                return Err(Error::CoseInvalidCert);
            }

            true
        }
        None => tbscert.is_ca(), // if is not ca it must be present
    };

    // populate needed extension info
    for e in signcert.extensions() {
        match e.parsed_extension() {
            ParsedExtension::AuthorityKeyIdentifier(_aki) => {
                aki_good = true;
            }
            ParsedExtension::SubjectKeyIdentifier(_spki) => {
                ski_good = true;
            }
            ParsedExtension::KeyUsage(ku) => {
                if ku.digital_signature() {
                    if ku.key_cert_sign() && !tbscert.is_ca() {
                        log_item!(
                            "Cose_Sign1",
                            "certificate missing digitalSignature EKU",
                            "check_cert_alg"
                        )
                        .validation_status(SIGNING_CREDENTIAL_INVALID)
                        .failure_no_throw(validation_log, Error::CoseInvalidCert);

                        return Err(Error::CoseInvalidCert);
                    }
                    key_usage_good = true;
                }
                if ku.key_cert_sign() || ku.non_repudiation() {
                    key_usage_good = true;
                }
                // todo: warn if not marked critical
                // if !e.critical { // warn here somehow}
            }
            ParsedExtension::CertificatePolicies(_) => (),
            ParsedExtension::PolicyMappings(_) => (),
            ParsedExtension::SubjectAlternativeName(_) => (),
            ParsedExtension::BasicConstraints(_) => (),
            ParsedExtension::NameConstraints(_) => (),
            ParsedExtension::PolicyConstraints(_) => (),
            ParsedExtension::ExtendedKeyUsage(_) => (),
            ParsedExtension::CRLDistributionPoints(_) => (),
            ParsedExtension::InhibitAnyPolicy(_) => (),
            ParsedExtension::AuthorityInfoAccess(_) => (),
            ParsedExtension::NSCertType(_) => (),
            ParsedExtension::CRLNumber(_) => (),
            ParsedExtension::ReasonCode(_) => (),
            ParsedExtension::InvalidityDate(_) => (),
            ParsedExtension::Unparsed => {
                if e.critical {
                    // unhandled critical extension
                    handled_all_critical = false;
                }
            }
            _ => {
                if e.critical {
                    // unhandled critical extension
                    handled_all_critical = false;
                }
            }
        }
    }

    // if cert is a CA must have valid SubjectKeyIdentifier
    ski_good = if tbscert.is_ca() { ski_good } else { true };

    // check all flags
    if aki_good && ski_good && key_usage_good && extended_key_usage_good && handled_all_critical {
        Ok(())
    } else {
        log_item!(
            "Cose_Sign1",
            "certificate params incorrect",
            "check_cert_alg"
        )
        .validation_status(SIGNING_CREDENTIAL_INVALID)
        .failure_no_throw(validation_log, Error::CoseInvalidCert);

        Err(Error::CoseInvalidCert)
    }
}

pub(crate) fn get_signing_alg(cs1: &coset::CoseSign1) -> Result<SigningAlg> {
    // find the supported handler for the algorithm
    match cs1.protected.header.alg {
        Some(ref alg) => match alg {
            coset::RegisteredLabelWithPrivate::PrivateUse(a) => match a {
                -39 => Ok(SigningAlg::Ps512),
                -38 => Ok(SigningAlg::Ps384),
                -37 => Ok(SigningAlg::Ps256),
                -36 => Ok(SigningAlg::Es512),
                -35 => Ok(SigningAlg::Es384),
                -7 => Ok(SigningAlg::Es256),
                -8 => Ok(SigningAlg::Ed25519),
                _ => Err(Error::CoseSignatureAlgorithmNotSupported),
            },
            coset::RegisteredLabelWithPrivate::Assigned(a) => match a {
                coset::iana::Algorithm::PS512 => Ok(SigningAlg::Ps512),
                coset::iana::Algorithm::PS384 => Ok(SigningAlg::Ps384),
                coset::iana::Algorithm::PS256 => Ok(SigningAlg::Ps256),
                coset::iana::Algorithm::ES512 => Ok(SigningAlg::Es512),
                coset::iana::Algorithm::ES384 => Ok(SigningAlg::Es384),
                coset::iana::Algorithm::ES256 => Ok(SigningAlg::Es256),
                coset::iana::Algorithm::EdDSA => Ok(SigningAlg::Ed25519),
                _ => Err(Error::CoseSignatureAlgorithmNotSupported),
            },
            coset::RegisteredLabelWithPrivate::Text(a) => a
                .parse()
                .map_err(|_| Error::CoseSignatureAlgorithmNotSupported),
        },
        None => Err(Error::CoseSignatureAlgorithmNotSupported),
    }
}

fn get_sign_cert(sign1: &coset::CoseSign1) -> Result<Vec<u8>> {
    // element 0 is the signing cert
    let certs = get_sign_certs(sign1)?;
    Ok(certs[0].clone())
}

fn get_unprotected_header_certs(sign1: &coset::CoseSign1) -> Result<Vec<Vec<u8>>> {
    if let Some(der) = sign1
        .unprotected
        .rest
        .iter()
        .find_map(|x: &(Label, Value)| {
            if x.0 == Label::Text("x5chain".to_string()) {
                Some(x.1.clone())
            } else {
                None
            }
        })
    {
        let mut certs: Vec<Vec<u8>> = Vec::new();

        match der {
            Value::Array(cert_chain) => {
                // handle array of certs
                for c in cert_chain {
                    if let Value::Bytes(der_bytes) = c {
                        certs.push(der_bytes.clone());
                    }
                }

                if certs.is_empty() {
                    Err(Error::CoseMissingKey)
                } else {
                    Ok(certs)
                }
            }
            Value::Bytes(ref der_bytes) => {
                // handle single cert case
                certs.push(der_bytes.clone());
                Ok(certs)
            }
            _ => Err(Error::CoseX5ChainMissing),
        }
    } else {
        Err(Error::CoseX5ChainMissing)
    }
}
// get the public key der
fn get_sign_certs(sign1: &coset::CoseSign1) -> Result<Vec<Vec<u8>>> {
    // check for protected header int, then protected header x5chain,
    // then the legacy unprotected x5chain to get the public key der

    // check the protected header
    if let Some(der) = sign1
        .protected
        .header
        .rest
        .iter()
        .find_map(|x: &(Label, Value)| {
            if x.0 == Label::Text("x5chain".to_string())
                || x.0 == Label::Int(iana::HeaderParameter::X5Chain.to_i64())
            {
                Some(x.1.clone())
            } else {
                None
            }
        })
    {
        // make sure there are no certs in the legacy unprotected header, certs
        // are only allowing in protect OR unprotected header
        if get_unprotected_header_certs(sign1).is_ok() {
            return Err(Error::CoseVerifier);
        }

        let mut certs: Vec<Vec<u8>> = Vec::new();

        match der {
            Value::Array(cert_chain) => {
                // handle array of certs
                for c in cert_chain {
                    if let Value::Bytes(der_bytes) = c {
                        certs.push(der_bytes.clone());
                    }
                }

                if certs.is_empty() {
                    return Err(Error::CoseX5ChainMissing);
                } else {
                    return Ok(certs);
                }
            }
            Value::Bytes(ref der_bytes) => {
                // handle single cert case
                certs.push(der_bytes.clone());
                return Ok(certs);
            }
            _ => return Err(Error::CoseX5ChainMissing),
        }
    }

    // check the unprotected header if necessary
    get_unprotected_header_certs(sign1)
}

// get OCSP der
fn get_ocsp_der(sign1: &coset::CoseSign1) -> Option<Vec<u8>> {
    if let Some(der) = sign1
        .unprotected
        .rest
        .iter()
        .find_map(|x: &(Label, Value)| {
            if x.0 == Label::Text("rVals".to_string()) {
                Some(x.1.clone())
            } else {
                None
            }
        })
    {
        match der {
            Value::Map(rvals_map) => {
                // find OCSP value if available
                rvals_map.iter().find_map(|x: &(Value, Value)| {
                    if x.0 == Value::Text("ocspVals".to_string()) {
                        x.1.as_array()
                            .and_then(|ocsp_rsp_val| ocsp_rsp_val.first())
                            .and_then(Value::as_bytes)
                            .cloned()
                    } else {
                        None
                    }
                })
            }
            _ => None,
        }
    } else {
        None
    }
}

#[allow(dead_code)]
#[async_generic]
pub(crate) fn check_ocsp_status(
    cose_bytes: &[u8],
    data: &[u8],
    cap: &CertificateAcceptancePolicy,
    validation_log: &mut impl StatusTracker,
) -> Result<OcspResponse> {
    let sign1 = get_cose_sign1(cose_bytes, data, validation_log)?;

    let mut result = Ok(OcspResponse::default());

    if let Some(ocsp_response_der) = get_ocsp_der(&sign1) {
        let time_stamp_info = if _sync {
            get_timestamp_info(&sign1, data)
        } else {
            get_timestamp_info_async(&sign1, data).await
        };

        // check stapled OCSP response, must have timestamp
        if let Ok(tst_info) = &time_stamp_info {
            let signing_time = gt_to_datetime(tst_info.gen_time.clone());

            // Check the OCSP response, only use if not malformed.  Revocation errors are reported in the validation log
            if let Ok(ocsp_data) = OcspResponse::from_der_checked(
                &ocsp_response_der,
                Some(signing_time),
                validation_log,
            ) {
                // if we get a valid response validate the certs
                if ocsp_data.revoked_at.is_none() {
                    if let Some(ocsp_certs) = &ocsp_data.ocsp_certs {
                        check_cert(&ocsp_certs[0], cap, validation_log, Some(tst_info))?;
                    }
                }
                result = Ok(ocsp_data);
            }
        }
    } else {
        #[cfg(not(target_arch = "wasm32"))]
        {
            // only support fetching with the enabled
            if let Ok(ocsp_fetch) = get_settings_value::<bool>("verify.ocsp_fetch") {
                if ocsp_fetch {
                    // get the cert chain
                    let certs = get_sign_certs(&sign1)?;

                    if let Some(ocsp_der) = c2pa_crypto::ocsp::fetch_ocsp_response(&certs) {
                        // fetch_ocsp_response(&certs) {
                        let ocsp_response_der = ocsp_der;

                        let time_stamp_info = get_timestamp_info(&sign1, data);

                        let signing_time = match &time_stamp_info {
                            Ok(tst_info) => {
                                let signing_time = gt_to_datetime(tst_info.gen_time.clone());
                                Some(signing_time)
                            }
                            Err(_) => None,
                        };

                        // Check the OCSP response, only use if not malformed.  Revocation errors are reported in the validation log
                        if let Ok(ocsp_data) = OcspResponse::from_der_checked(
                            &ocsp_response_der,
                            signing_time,
                            validation_log,
                        ) {
                            // if we get a valid response validate the certs
                            if ocsp_data.revoked_at.is_none() {
                                if let Some(ocsp_certs) = &ocsp_data.ocsp_certs {
                                    check_cert(&ocsp_certs[0], cap, validation_log, None)?;
                                }
                            }
                            result = Ok(ocsp_data);
                        }
                    }
                }
            }
        }
    }

    result
}

// internal util function to dump the cert chain in PEM format
fn dump_cert_chain(certs: &[Vec<u8>]) -> Result<Vec<u8>> {
    let mut out_buf: Vec<u8> = Vec::new();
    let mut writer = Cursor::new(out_buf);

    for der_bytes in certs {
        let c = x509_certificate::X509Certificate::from_der(der_bytes)
            .map_err(|_e| Error::UnsupportedType)?;
        c.write_pem(&mut writer)?;
    }
    out_buf = writer.into_inner();
    Ok(out_buf)
}

// Note: this function is only used to get the display string and not for cert validation.
#[async_generic]
fn get_signing_time(
    sign1: &coset::CoseSign1,
    data: &[u8],
) -> Option<chrono::DateTime<chrono::Utc>> {
    // get timestamp info if available

    let time_stamp_info = if _sync {
        get_timestamp_info(sign1, data)
    } else {
        get_timestamp_info_async(sign1, data).await
    };

    if let Ok(tst_info) = time_stamp_info {
        Some(gt_to_datetime(tst_info.gen_time))
    } else {
        None
    }
}

// return appropriate TstInfo if available
#[async_generic]
fn get_timestamp_info(sign1: &coset::CoseSign1, data: &[u8]) -> Result<TstInfo> {
    // parse the temp timestamp
    if let Some(t) = &sign1
        .unprotected
        .rest
        .iter()
        .find_map(|x: &(Label, Value)| {
            if x.0 == Label::Text("sigTst".to_string()) {
                Some(x.1.clone())
            } else {
                None
            }
        })
    {
        let time_cbor = serde_cbor::to_vec(t)?;
        let tst_infos = if _sync {
            parse_and_validate_sigtst(&time_cbor, data, &sign1.protected)?
        } else {
            parse_and_validate_sigtst_async(&time_cbor, data, &sign1.protected).await?
        };

        // there should only be one but consider handling more in the future since it is technically ok
        if !tst_infos.is_empty() {
            return Ok(tst_infos[0].clone());
        }
    }
    Err(Error::NotFound)
}

#[async_generic(async_signature(
    cap: &CertificateAcceptancePolicy,
    chain_der: &[Vec<u8>],
    cert_der: &[u8],
    signing_time_epoc: Option<i64>,
    validation_log: &mut impl StatusTracker
))]
#[allow(unused)]
fn check_trust(
    cap: &CertificateAcceptancePolicy,
    chain_der: &[Vec<u8>],
    cert_der: &[u8],
    signing_time_epoc: Option<i64>,
    validation_log: &mut impl StatusTracker,
) -> Result<()> {
    // just return is trust checks are disabled or misconfigured
    match get_settings_value::<bool>("verify.verify_trust") {
        Ok(verify_trust) => {
            if !verify_trust {
                return Ok(());
            }
        }
        Err(e) => return Err(e),
    }

    // is the certificate trusted

    let verify_result: Result<bool> = if _sync {
        #[cfg(not(feature = "openssl"))]
        {
            Err(Error::NotImplemented(
                "no trust handler for this feature".to_string(),
            ))
        }

        #[cfg(feature = "openssl")]
        {
            verify_trust(cap, chain_der, cert_der, signing_time_epoc)
        }
    } else {
        #[cfg(target_arch = "wasm32")]
        {
            verify_trust_async(cap, chain_der, cert_der, signing_time_epoc).await
        }

        #[cfg(feature = "openssl")]
        {
            verify_trust(cap, chain_der, cert_der, signing_time_epoc)
        }

        #[cfg(not(any(feature = "openssl", target_arch = "wasm32")))]
        {
            Err(Error::NotImplemented(
                "no trust handler for this feature".to_string(),
            ))
        }
    };

    match verify_result {
        Ok(trusted) => {
            if trusted {
                log_item!("Cose_Sign1", "signing certificate trusted", "verify_cose")
                    .validation_status(SIGNING_CREDENTIAL_TRUSTED)
                    .success(validation_log);

                Ok(())
            } else {
                log_item!("Cose_Sign1", "signing certificate untrusted", "verify_cose")
                    .validation_status(SIGNING_CREDENTIAL_UNTRUSTED)
                    .failure_no_throw(validation_log, Error::CoseCertUntrusted);

                Err(Error::CoseCertUntrusted)
            }
        }
        Err(e) => {
            log_item!("Cose_Sign1", "signing certificate untrusted", "verify_cose")
                .validation_status(SIGNING_CREDENTIAL_UNTRUSTED)
                .failure_no_throw(validation_log, &e);

            // TO REVIEW: Mixed message: Are we using CoseCertUntrusted in log or &e from above?
            // validation_log.log(log_item, Error::CoseCertUntrusted)?;
            Err(e)
        }
    }
}

// test for unrecognized signatures
fn check_sig(sig: &[u8], alg: SigningAlg) -> Result<()> {
    match alg {
        SigningAlg::Es256 | SigningAlg::Es384 | SigningAlg::Es512 => {
            if parse_ec_der_sig(sig).is_ok() {
                // expected P1363 format
                return Err(Error::InvalidEcdsaSignature);
            }
        }
        _ => (),
    }
    Ok(())
}

/// A wrapper containing information of the signing cert.
pub(crate) struct CertInfo {
    /// The name of the identity the certificate is issued to.
    pub subject: String,
    /// The serial number of the cert. Will be unique to the CA.
    pub serial_number: BigUint,
}

fn extract_subject_from_cert(cert: &X509Certificate) -> Result<String> {
    cert.subject()
        .iter_organization()
        .map(|attr| attr.as_str())
        .last()
        .ok_or(Error::CoseX5ChainMissing)?
        .map(|attr| attr.to_string())
        .map_err(|_e| Error::CoseX5ChainMissing)
}

/// Returns the unique serial number from the provided cert.
fn extract_serial_from_cert(cert: &X509Certificate) -> BigUint {
    cert.serial.clone()
}

fn tst_info_result_to_timestamp(tst_info_res: &Result<TstInfo>) -> Option<i64> {
    match &tst_info_res {
        Ok(tst_info) => {
            let dt: chrono::DateTime<chrono::Utc> = tst_info.gen_time.clone().into();
            Some(dt.timestamp())
        }
        Err(_) => None,
    }
}

/// Asynchronously validate a COSE_SIGN1 byte vector and verify against expected data
/// cose_bytes - byte array containing the raw COSE_SIGN1 data
/// data:  data that was used to create the cose_bytes, these must match
/// addition_data: additional optional data that may have been used during signing
/// returns - Ok on success
pub(crate) async fn verify_cose_async(
    cose_bytes: Vec<u8>,
    data: Vec<u8>,
    additional_data: Vec<u8>,
    cert_check: bool,
    cap: &CertificateAcceptancePolicy,
    validation_log: &mut impl StatusTracker,
) -> Result<ValidationInfo> {
    let mut sign1 = get_cose_sign1(&cose_bytes, &data, validation_log)?;

    let alg = match get_signing_alg(&sign1) {
        Ok(a) => a,
        Err(_) => {
            log_item!(
                "Cose_Sign1",
                "unsupported or missing Cose algorithm",
                "verify_cose_async"
            )
            .validation_status(ALGORITHM_UNSUPPORTED)
            .failure_no_throw(validation_log, Error::CoseSignatureAlgorithmNotSupported);

            // one of these must exist
            return Err(Error::CoseSignatureAlgorithmNotSupported);
        }
    };

    // build result structure
    let mut result = ValidationInfo::default();

    // get the cert chain
    let certs = get_sign_certs(&sign1)?;

    // get the public key der
    let der_bytes = &certs[0];

    let tst_info_res = get_timestamp_info_async(&sign1, &data).await;

    // verify cert matches requested algorithm
    if cert_check {
        // verify certs
        match &tst_info_res {
            Ok(tst_info) => check_cert(der_bytes, cap, validation_log, Some(tst_info))?,
            Err(e) => {
                // log timestamp errors
                match e {
                    Error::NotFound => check_cert(der_bytes, cap, validation_log, None)?,

                    Error::TimeStampError(TimeStampError::InvalidData) => {
                        log_item!(
                            "Cose_Sign1",
                            "timestamp message imprint did not match",
                            "verify_cose"
                        )
                        .validation_status(TIMESTAMP_MISMATCH)
                        .failure(validation_log, Error::CoseTimeStampMismatch)?;
                    }

                    Error::CoseTimeStampValidity => {
                        log_item!("Cose_Sign1", "timestamp outside of validity", "verify_cose")
                            .validation_status(TIMESTAMP_OUTSIDE_VALIDITY)
                            .failure(validation_log, Error::CoseTimeStampValidity)?;
                    }

                    _ => {
                        log_item!("Cose_Sign1", "error parsing timestamp", "verify_cose")
                            .failure_no_throw(validation_log, Error::CoseInvalidTimeStamp);

                        return Err(Error::CoseInvalidTimeStamp);
                    }
                }
            }
        }

        // is the certificate trusted
        #[cfg(target_arch = "wasm32")]
        check_trust_async(
            cap,
            &certs[1..],
            der_bytes,
            tst_info_result_to_timestamp(&tst_info_res),
            validation_log,
        )
        .await?;

        #[cfg(not(target_arch = "wasm32"))]
        check_trust(
            cap,
            &certs[1..],
            der_bytes,
            tst_info_result_to_timestamp(&tst_info_res),
            validation_log,
        )?;

        // todo: check TSA certs against trust list
    }

    // check signature format
    if let Err(_e) = check_sig(&sign1.signature, alg) {
        log_item!("Cose_Sign1", "unsupported signature format", "verify_cose")
            .validation_status(SIGNING_CREDENTIAL_INVALID)
            .failure_no_throw(validation_log, Error::CoseSignatureAlgorithmNotSupported);

        // TO REVIEW: This could return e if OneShotStatusTracker is used. Hmmm.
        // validation_log.log(log_item, e)?;

        return Err(Error::CoseSignatureAlgorithmNotSupported);
    }

    // Check the signature, which needs to have the same `additional_data` provided, by
    // providing a closure that can do the verify operation.
    sign1.payload = Some(data.clone()); // restore payload

    let p_header = sign1.protected.clone();

    let tbs = sig_structure_data(
        coset::SignatureContext::CoseSign1,
        p_header,
        None,
        &additional_data,
        sign1.payload.as_ref().unwrap_or(&vec![]),
    ); // get "to be signed" bytes

    if let Ok(CertInfo {
        subject,
        serial_number,
    }) = validate_with_cert_async(alg, &sign1.signature, &tbs, der_bytes).await
    {
        result.issuer_org = Some(subject);
        result.cert_serial_number = Some(serial_number);
        result.validated = true;
        result.alg = Some(alg);

        result.date = tst_info_res.map(|t| gt_to_datetime(t.gen_time)).ok();

        // return cert chain
        result.cert_chain = dump_cert_chain(&get_sign_certs(&sign1)?)?;
    }

    Ok(result)
}

#[allow(unused_variables)]
#[async_generic]
pub(crate) fn get_signing_info(
    cose_bytes: &[u8],
    data: &[u8],
    validation_log: &mut impl StatusTracker,
) -> ValidationInfo {
    let mut date = None;
    let mut issuer_org = None;
    let mut alg: Option<SigningAlg> = None;
    let mut cert_serial_number = None;

    let sign1 = match get_cose_sign1(cose_bytes, data, validation_log) {
        Ok(sign1) => {
            // get the public key der
            match get_sign_cert(&sign1) {
                Ok(der_bytes) => {
                    if let Ok((_rem, signcert)) = X509Certificate::from_der(&der_bytes) {
                        date = if _sync {
                            get_signing_time(&sign1, data)
                        } else {
                            get_signing_time_async(&sign1, data).await
                        };
                        issuer_org = extract_subject_from_cert(&signcert).ok();
                        cert_serial_number = Some(extract_serial_from_cert(&signcert));
                        if let Ok(a) = get_signing_alg(&sign1) {
                            alg = Some(a);
                        }
                    };

                    Ok(sign1)
                }
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    };

    let certs = match sign1 {
        Ok(s) => match get_sign_certs(&s) {
            Ok(c) => dump_cert_chain(&c).unwrap_or_default(),
            Err(_) => Vec::new(),
        },
        Err(_e) => Vec::new(),
    };

    ValidationInfo {
        issuer_org,
        date,
        alg,
        validated: false,
        cert_chain: certs,
        cert_serial_number,
        revocation_status: None,
    }
}

/// Validate a COSE_SIGN1 byte vector and verify against expected data
/// cose_bytes - byte array containing the raw COSE_SIGN1 data
/// data:  data that was used to create the cose_bytes, these must match
/// addition_data: additional optional data that may have been used during signing
/// returns - Ok on success
pub(crate) fn verify_cose(
    cose_bytes: &[u8],
    data: &[u8],
    additional_data: &[u8],
    cert_check: bool,
    cap: &CertificateAcceptancePolicy,
    validation_log: &mut impl StatusTracker,
) -> Result<ValidationInfo> {
    let sign1 = get_cose_sign1(cose_bytes, data, validation_log)?;

    let alg = match get_signing_alg(&sign1) {
        Ok(a) => a,
        Err(_) => {
            log_item!(
                "Cose_Sign1",
                "unsupported or missing Cose algorithm",
                "verify_cose"
            )
            .validation_status(ALGORITHM_UNSUPPORTED)
            .failure_no_throw(validation_log, Error::CoseSignatureAlgorithmNotSupported);

            return Err(Error::CoseSignatureAlgorithmNotSupported);
        }
    };

    let Some(validator) = validator_for_signing_alg(alg) else {
        return Err(Error::CoseSignatureAlgorithmNotSupported);
    };

    // build result structure
    let mut result = ValidationInfo::default();

    // get the cert chain
    let certs = get_sign_certs(&sign1)?;

    // get the public key der
    let der_bytes = &certs[0];

    let tst_info_res = get_timestamp_info(&sign1, data);

    if cert_check {
        // verify certs
        match &tst_info_res {
            Ok(tst_info) => check_cert(der_bytes, cap, validation_log, Some(tst_info))?,
            Err(e) => {
                // log timestamp errors
                match e {
                    Error::NotFound => check_cert(der_bytes, cap, validation_log, None)?,

                    Error::TimeStampError(TimeStampError::InvalidData) => {
                        log_item!(
                            "Cose_Sign1",
                            "timestamp did not match signed data",
                            "verify_cose"
                        )
                        .validation_status(TIMESTAMP_MISMATCH)
                        .failure_no_throw(validation_log, Error::CoseTimeStampMismatch);

                        return Err(Error::CoseTimeStampMismatch);
                    }

                    Error::CoseTimeStampValidity => {
                        log_item!(
                            "Cose_Sign1",
                            "timestamp certificate outside of validity",
                            "verify_cose"
                        )
                        .validation_status(TIMESTAMP_OUTSIDE_VALIDITY)
                        .failure_no_throw(validation_log, Error::CoseTimeStampValidity);

                        return Err(Error::CoseTimeStampValidity);
                    }

                    _ => {
                        log_item!("Cose_Sign1", "error parsing timestamp", "verify_cose")
                            .failure_no_throw(validation_log, Error::CoseInvalidTimeStamp);

                        return Err(Error::CoseInvalidTimeStamp);
                    }
                }
            }
        }

        // is the certificate trusted
        check_trust(
            cap,
            &certs[1..],
            der_bytes,
            tst_info_result_to_timestamp(&tst_info_res),
            validation_log,
        )?;

        // todo: check TSA certs against trust list
    }

    // check signature format
    if let Err(e) = check_sig(&sign1.signature, alg) {
        log_item!("Cose_Sign1", "unsupported signature format", "verify_cose")
            .validation_status(SIGNING_CREDENTIAL_INVALID)
            .failure_no_throw(validation_log, e);

        return Err(Error::CoseSignatureAlgorithmNotSupported);
    }

    // Check the signature, which needs to have the same `additional_data` provided, by
    // providing a closure that can do the verify operation.
    sign1.verify_signature(additional_data, |sig, verify_data| -> Result<()> {
        if let Ok(CertInfo {
            subject,
            serial_number,
        }) = validate_with_cert(validator, sig, verify_data, der_bytes)
        {
            result.issuer_org = Some(subject);
            result.cert_serial_number = Some(serial_number);
            result.validated = true;
            result.alg = Some(alg);

            result.date = tst_info_res.map(|t| gt_to_datetime(t.gen_time)).ok();

            // return cert chain
            result.cert_chain = dump_cert_chain(&certs)?;

            result.revocation_status = Some(true);
        }
        // Note: not adding validation_log entry here since caller will supply claim specific info to log
        Ok(())
    })?;

    Ok(result)
}

fn validate_with_cert(
    validator: Box<dyn RawSignatureValidator>,
    sig: &[u8],
    data: &[u8],
    der_bytes: &[u8],
) -> Result<CertInfo> {
    // get the cert in der format
    let (_rem, signcert) =
        X509Certificate::from_der(der_bytes).map_err(|_err| Error::CoseInvalidCert)?;
    let pk = signcert.public_key();
    let pk_der = pk.raw;

    validator.validate(sig, data, pk_der)?;

    Ok(CertInfo {
        subject: extract_subject_from_cert(&signcert).unwrap_or_default(),
        serial_number: extract_serial_from_cert(&signcert),
    })
}

#[cfg(target_arch = "wasm32")]
async fn validate_with_cert_async(
    signing_alg: SigningAlg,
    sig: &[u8],
    data: &[u8],
    der_bytes: &[u8],
) -> Result<CertInfo> {
    let (_rem, signcert) =
        X509Certificate::from_der(der_bytes).map_err(|_err| Error::CoseMissingKey)?;
    let pk = signcert.public_key();
    let pk_der = pk.raw;

    let Some(validator) = c2pa_crypto::webcrypto::async_validator_for_signing_alg(signing_alg)
    else {
        return Err(Error::UnknownAlgorithm);
    };

    validator.validate_async(sig, data, pk_der).await?;

    Ok(CertInfo {
        subject: extract_subject_from_cert(&signcert).unwrap_or_default(),
        serial_number: extract_serial_from_cert(&signcert),
    })
}

#[cfg(not(target_arch = "wasm32"))]
async fn validate_with_cert_async(
    signing_alg: SigningAlg,
    sig: &[u8],
    data: &[u8],
    der_bytes: &[u8],
) -> Result<CertInfo> {
    let Some(validator) = validator_for_signing_alg(signing_alg) else {
        return Err(Error::CoseSignatureAlgorithmNotSupported);
    };

    validate_with_cert(validator, sig, data, der_bytes)
}

fn gt_to_datetime(
    gt: x509_certificate::asn1time::GeneralizedTime,
) -> chrono::DateTime<chrono::Utc> {
    gt.into()
}

#[allow(unused_imports)]
#[allow(clippy::unwrap_used)]
#[cfg(feature = "openssl_sign")]
#[cfg(test)]
pub mod tests {
    use c2pa_crypto::SigningAlg;
    use c2pa_status_tracker::DetailedStatusTracker;
    use sha2::digest::generic_array::sequence::Shorten;

    use super::*;
    use crate::{utils::test_signer::test_signer, Signer};

    #[test]
    #[cfg(feature = "file_io")]
    fn test_expired_cert() {
        let mut validation_log = DetailedStatusTracker::default();
        let cap = CertificateAcceptancePolicy::default();

        let mut cert_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        cert_path.push("tests/fixtures/rsa-pss256_key-expired.pub");

        let expired_cert = std::fs::read(&cert_path).unwrap();

        if let Ok(signcert) = openssl::x509::X509::from_pem(&expired_cert) {
            let der_bytes = signcert.to_der().unwrap();
            assert!(check_cert(&der_bytes, &cap, &mut validation_log, None).is_err());

            assert!(!validation_log.logged_items().is_empty());

            assert_eq!(
                validation_log.logged_items()[0].validation_status,
                Some(SIGNING_CREDENTIAL_EXPIRED.into())
            );
        }
    }

    #[test]
    #[cfg(all(feature = "openssl_sign", feature = "file_io"))]
    fn test_cert_algorithms() {
        let cap = CertificateAcceptancePolicy::default();

        let mut validation_log = DetailedStatusTracker::default();

        let es256_cert = include_bytes!("../tests/fixtures/certs/es256.pub");
        let es384_cert = include_bytes!("../tests/fixtures/certs/es384.pub");
        let es512_cert = include_bytes!("../tests/fixtures/certs/es512.pub");
        let rsa_pss256_cert = include_bytes!("../tests/fixtures/certs/ps256.pub");

        if let Ok(signcert) = openssl::x509::X509::from_pem(es256_cert) {
            let der_bytes = signcert.to_der().unwrap();
            assert!(check_cert(&der_bytes, &cap, &mut validation_log, None).is_ok());
        }

        if let Ok(signcert) = openssl::x509::X509::from_pem(es384_cert) {
            let der_bytes = signcert.to_der().unwrap();
            assert!(check_cert(&der_bytes, &cap, &mut validation_log, None).is_ok());
        }

        if let Ok(signcert) = openssl::x509::X509::from_pem(es512_cert) {
            let der_bytes = signcert.to_der().unwrap();
            assert!(check_cert(&der_bytes, &cap, &mut validation_log, None).is_ok());
        }

        if let Ok(signcert) = openssl::x509::X509::from_pem(rsa_pss256_cert) {
            let der_bytes = signcert.to_der().unwrap();
            assert!(check_cert(&der_bytes, &cap, &mut validation_log, None).is_ok());
        }
    }

    #[test]
    fn test_no_timestamp() {
        let mut validation_log = DetailedStatusTracker::default();

        let mut claim = crate::claim::Claim::new("extern_sign_test", Some("contentauth"));
        claim.build().unwrap();

        let claim_bytes = claim.data().unwrap();

        let box_size = 10000;

        let signer = test_signer(SigningAlg::Ps256);

        let cose_bytes =
            crate::cose_sign::sign_claim(&claim_bytes, signer.as_ref(), box_size).unwrap();

        let cose_sign1 = get_cose_sign1(&cose_bytes, &claim_bytes, &mut validation_log).unwrap();

        let signing_time = get_signing_time(&cose_sign1, &claim_bytes);

        assert_eq!(signing_time, None);
    }
    #[test]
    #[cfg(feature = "openssl_sign")]
    fn test_stapled_ocsp() {
        use c2pa_crypto::raw_signature::{
            signer_from_cert_chain_and_private_key, RawSigner, RawSignerError,
        };

        let mut validation_log = DetailedStatusTracker::default();

        let mut claim = crate::claim::Claim::new("ocsp_sign_test", Some("contentauth"));
        claim.build().unwrap();

        let claim_bytes = claim.data().unwrap();

        let sign_cert = include_bytes!("../tests/fixtures/certs/ps256.pub").to_vec();
        let pem_key = include_bytes!("../tests/fixtures/certs/ps256.pem").to_vec();
        let ocsp_rsp_data = include_bytes!("../tests/fixtures/ocsp_good.data");

        let signer =
            signer_from_cert_chain_and_private_key(&sign_cert, &pem_key, SigningAlg::Ps256, None)
                .unwrap();

        // create a test signer that supports stapling
        struct OcspSigner {
            pub signer: Box<dyn crate::Signer>,
            pub ocsp_rsp: Vec<u8>,
        }

        impl crate::Signer for OcspSigner {
            fn sign(&self, data: &[u8]) -> Result<Vec<u8>> {
                self.signer.sign(data)
            }

            fn alg(&self) -> SigningAlg {
                SigningAlg::Ps256
            }

            fn certs(&self) -> Result<Vec<Vec<u8>>> {
                self.signer.certs()
            }

            fn reserve_size(&self) -> usize {
                self.signer.reserve_size()
            }

            fn ocsp_val(&self) -> Option<Vec<u8>> {
                Some(self.ocsp_rsp.clone())
            }
        }

        let ocsp_signer = OcspSigner {
            signer: Box::new(crate::signer::RawSignerWrapper(signer)),
            ocsp_rsp: ocsp_rsp_data.to_vec(),
        };

        // sign and staple
        let cose_bytes =
            crate::cose_sign::sign_claim(&claim_bytes, &ocsp_signer, ocsp_signer.reserve_size())
                .unwrap();

        let cose_sign1 = get_cose_sign1(&cose_bytes, &claim_bytes, &mut validation_log).unwrap();
        let ocsp_stapled = get_ocsp_der(&cose_sign1).unwrap();

        assert_eq!(ocsp_rsp_data, ocsp_stapled.as_slice());
    }
}
