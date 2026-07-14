use openssl::{
    error::ErrorStack,
    hash::MessageDigest,
    pkey::{PKey, Public},
    rsa::Padding,
    sign::{RsaPssSaltlen, Verifier},
    x509::X509,
};
use rustls::SignatureScheme;

#[derive(Debug)]
pub enum PinError {
    OpenSsl(ErrorStack),
    UnsupportedSignatureScheme(SignatureScheme),
    SignatureMismatch,
}

impl From<ErrorStack> for PinError {
    fn from(value: ErrorStack) -> Self {
        Self::OpenSsl(value)
    }
}

#[derive(Clone)]
pub struct PinnedTlsIdentity {
    expected_certificate_der: Vec<u8>,
    public_key: PKey<Public>,
}

impl PinnedTlsIdentity {
    pub fn from_certificate_der(certificate_der: &[u8]) -> Result<Self, PinError> {
        let certificate = X509::from_der(certificate_der)?;
        let public_key = certificate.public_key()?;
        Ok(Self {
            expected_certificate_der: certificate_der.to_vec(),
            public_key,
        })
    }

    pub fn certificate_matches(&self, presented_certificate_der: &[u8]) -> bool {
        self.expected_certificate_der == presented_certificate_der
    }

    pub fn verify_handshake_signature(
        &self,
        message: &[u8],
        scheme: SignatureScheme,
        signature: &[u8],
    ) -> Result<(), PinError> {
        let (digest, padding) = signature_parameters(scheme)?;
        let mut verifier = match digest {
            Some(digest) => Verifier::new(digest, &self.public_key)?,
            None => Verifier::new_without_digest(&self.public_key)?,
        };

        if let Some(padding) = padding {
            verifier.set_rsa_padding(padding)?;
            if padding == Padding::PKCS1_PSS {
                let digest = digest.expect("RSA-PSS always has a digest");
                verifier.set_rsa_mgf1_md(digest)?;
                verifier.set_rsa_pss_saltlen(RsaPssSaltlen::DIGEST_LENGTH)?;
            }
        }

        if verifier.verify_oneshot(signature, message)? {
            Ok(())
        } else {
            Err(PinError::SignatureMismatch)
        }
    }
}

fn signature_parameters(
    scheme: SignatureScheme,
) -> Result<(Option<MessageDigest>, Option<Padding>), PinError> {
    use SignatureScheme::*;

    let value = match scheme {
        RSA_PKCS1_SHA1 => (Some(MessageDigest::sha1()), Some(Padding::PKCS1)),
        RSA_PKCS1_SHA256 => (Some(MessageDigest::sha256()), Some(Padding::PKCS1)),
        RSA_PKCS1_SHA384 => (Some(MessageDigest::sha384()), Some(Padding::PKCS1)),
        RSA_PKCS1_SHA512 => (Some(MessageDigest::sha512()), Some(Padding::PKCS1)),
        RSA_PSS_SHA256 => (Some(MessageDigest::sha256()), Some(Padding::PKCS1_PSS)),
        RSA_PSS_SHA384 => (Some(MessageDigest::sha384()), Some(Padding::PKCS1_PSS)),
        RSA_PSS_SHA512 => (Some(MessageDigest::sha512()), Some(Padding::PKCS1_PSS)),
        ECDSA_SHA1_Legacy => (Some(MessageDigest::sha1()), None),
        ECDSA_NISTP256_SHA256 => (Some(MessageDigest::sha256()), None),
        ECDSA_NISTP384_SHA384 => (Some(MessageDigest::sha384()), None),
        ECDSA_NISTP521_SHA512 => (Some(MessageDigest::sha512()), None),
        ED25519 | ED448 => (None, None),
        _ => return Err(PinError::UnsupportedSignatureScheme(scheme)),
    };
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::{PinError, PinnedTlsIdentity};
    use openssl::{
        asn1::{Asn1Integer, Asn1Time},
        bn::BigNum,
        hash::MessageDigest,
        pkey::{PKey, Private},
        rsa::{Padding, Rsa},
        sign::{RsaPssSaltlen, Signer},
        x509::{X509, X509NameBuilder},
    };
    use rustls::SignatureScheme;

    fn rsa_certificate() -> (Vec<u8>, PKey<Private>) {
        let key = PKey::from_rsa(Rsa::generate(2048).expect("RSA key generation must succeed"))
            .expect("RSA key conversion must succeed");
        let mut name = X509NameBuilder::new().expect("X509 name builder must be created");
        name.append_entry_by_text("CN", "Sunshine")
            .expect("CN must be accepted");
        let name = name.build();

        let mut builder = X509::builder().expect("X509 builder must be created");
        builder.set_version(2).expect("X509 version must be set");
        let serial = Asn1Integer::from_bn(&BigNum::from_u32(0).expect("serial bignum"))
            .expect("serial integer");
        builder
            .set_serial_number(&serial)
            .expect("zero serial should be representable by OpenSSL");
        builder
            .set_subject_name(&name)
            .expect("subject must be set");
        builder.set_issuer_name(&name).expect("issuer must be set");
        builder.set_pubkey(&key).expect("public key must be set");
        builder
            .set_not_before(Asn1Time::days_from_now(0).expect("not before").as_ref())
            .expect("not before must be set");
        builder
            .set_not_after(Asn1Time::days_from_now(1).expect("not after").as_ref())
            .expect("not after must be set");
        builder
            .sign(&key, MessageDigest::sha256())
            .expect("certificate signing must succeed");
        (builder.build().to_der().expect("certificate DER"), key)
    }

    fn sign_rsa(
        key: &PKey<Private>,
        message: &[u8],
        digest: MessageDigest,
        padding: Padding,
    ) -> Vec<u8> {
        let mut signer = Signer::new(digest, key).expect("signer must be created");
        signer
            .set_rsa_padding(padding)
            .expect("RSA padding must be configured");
        if padding == Padding::PKCS1_PSS {
            signer
                .set_rsa_mgf1_md(digest)
                .expect("PSS MGF1 digest must be configured");
            signer
                .set_rsa_pss_saltlen(RsaPssSaltlen::DIGEST_LENGTH)
                .expect("PSS salt length must be configured");
        }
        signer
            .sign_oneshot_to_vec(message)
            .expect("signing must succeed")
    }

    #[test]
    fn exact_certificate_der_is_required() {
        let (certificate, _) = rsa_certificate();
        let identity = PinnedTlsIdentity::from_certificate_der(&certificate)
            .expect("pinned identity must parse a zero-serial Sunshine-style certificate");
        assert!(identity.certificate_matches(&certificate));

        let mut different = certificate.clone();
        let final_byte = different.last_mut().expect("DER must not be empty");
        *final_byte ^= 1;
        assert!(!identity.certificate_matches(&different));
    }

    #[test]
    fn verifies_rsa_pkcs1_tls12_signature_and_rejects_wrong_message() {
        let (certificate, key) = rsa_certificate();
        let identity = PinnedTlsIdentity::from_certificate_der(&certificate).expect("identity");
        let message = b"TLS 1.2 CertificateVerify transcript";
        let signature = sign_rsa(&key, message, MessageDigest::sha256(), Padding::PKCS1);

        identity
            .verify_handshake_signature(message, SignatureScheme::RSA_PKCS1_SHA256, &signature)
            .expect("valid PKCS1 signature must verify");
        assert!(matches!(
            identity.verify_handshake_signature(
                b"tampered transcript",
                SignatureScheme::RSA_PKCS1_SHA256,
                &signature,
            ),
            Err(PinError::SignatureMismatch)
        ));
    }

    #[test]
    fn verifies_rsa_pss_tls13_signature_and_rejects_tampering() {
        let (certificate, key) = rsa_certificate();
        let identity = PinnedTlsIdentity::from_certificate_der(&certificate).expect("identity");
        let message = b"TLS 1.3 CertificateVerify transcript";
        let signature = sign_rsa(&key, message, MessageDigest::sha256(), Padding::PKCS1_PSS);

        identity
            .verify_handshake_signature(message, SignatureScheme::RSA_PSS_SHA256, &signature)
            .expect("valid PSS signature must verify");

        let mut tampered = signature;
        tampered[0] ^= 1;
        assert!(matches!(
            identity.verify_handshake_signature(
                message,
                SignatureScheme::RSA_PSS_SHA256,
                &tampered,
            ),
            Err(PinError::SignatureMismatch)
        ));
    }

    #[test]
    fn rejects_signature_schemes_without_an_openssl_mapping() {
        let (certificate, _) = rsa_certificate();
        let identity = PinnedTlsIdentity::from_certificate_der(&certificate).expect("identity");
        assert!(matches!(
            identity.verify_handshake_signature(
                b"message",
                SignatureScheme::ML_DSA_44,
                b"signature",
            ),
            Err(PinError::UnsupportedSignatureScheme(
                SignatureScheme::ML_DSA_44
            ))
        ));
    }
}
