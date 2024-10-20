//! # JSON Web Signature (JWS)
//!
//! JWS ([RFC7515]) represents content secured with digital signatures using
//! JSON-based data structures. Cryptographic algorithms and identifiers for use
//! with this specification are described in the JWA ([RFC7518]) specification.
//!
//! [RFC7515]: https://www.rfc-editor.org/rfc/rfc7515
//! [RFC7518]: https://www.rfc-editor.org/rfc/rfc7518

use std::future::Future;

use anyhow::{anyhow, bail};
use base64ct::{Base64UrlUnpadded, Encoding};
use ecdsa::signature::Verifier as _;
use serde::de::DeserializeOwned;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize};

use crate::jose::jwk::PublicKeyJwk;
pub use crate::jose::jwt::Jwt;
pub use crate::jose::{KeyType, Type};
use crate::{Algorithm, Curve, Signer};

/// Encode the provided header and claims and sign, returning a JWT in compact
/// JWS form.
///
/// # Errors
/// TODO: document errors
pub async fn encode<T>(typ: Type, claims: &T, signer: impl Signer) -> anyhow::Result<String>
where
    T: Serialize + Send + Sync,
{
    tracing::debug!("encode");

    // header
    let header = Protected {
        alg: signer.algorithm(),
        typ,
        key: KeyType::KeyId(signer.verification_method()),
        ..Protected::default()
    };

    // payload
    let header = Base64UrlUnpadded::encode_string(&serde_json::to_vec(&header)?);
    let claims = Base64UrlUnpadded::encode_string(&serde_json::to_vec(claims)?);
    let payload = format!("{header}.{claims}");

    // sign
    let sig = signer.try_sign(payload.as_bytes()).await?;
    let sig_enc = Base64UrlUnpadded::encode_string(&sig);

    Ok(format!("{payload}.{sig_enc}"))
}

// TODO: allow passing verifier into this method

/// Decode the JWT token and return the claims.
///
/// # Errors
/// TODO: document errors
pub async fn decode<F, Fut, T>(token: &str, resolver: F) -> anyhow::Result<Jwt<T>>
where
    T: DeserializeOwned + Send,
    F: FnOnce(String) -> Fut + Send + Sync,
    Fut: Future<Output = anyhow::Result<PublicKeyJwk>> + Send + Sync,
{
    // TODO: cater for different key types
    let parts = token.split('.').collect::<Vec<&str>>();
    if parts.len() != 3 {
        bail!("invalid Compact JWS format");
    }

    // deserialize header, claims, and signature
    let decoded = Base64UrlUnpadded::decode_vec(parts[0])
        .map_err(|e| anyhow!("issue decoding header: {e}"))?;
    let header: Protected =
        serde_json::from_slice(&decoded).map_err(|e| anyhow!("issue deserializing header: {e}"))?;
    let decoded = Base64UrlUnpadded::decode_vec(parts[1])
        .map_err(|e| anyhow!("issue decoding claims: {e}"))?;
    let claims =
        serde_json::from_slice(&decoded).map_err(|e| anyhow!("issue deserializing claims:{e}"))?;
    let sig = Base64UrlUnpadded::decode_vec(parts[2])
        .map_err(|e| anyhow!("issue decoding signature: {e}"))?;

    // check algorithm
    if !(header.alg == Algorithm::ES256K || header.alg == Algorithm::EdDSA) {
        bail!("'alg' is not recognised");
    }

    // verify signature
    let KeyType::KeyId(kid) = header.key.clone() else {
        bail!("'kid' is not set");
    };

    // resolve 'kid' to Jwk (hint: kid will contain a DID URL for now)
    let jwk = resolver(kid).await?;
    jwk.verify(&format!("{}.{}", parts[0], parts[1]), &sig)?;

    Ok(Jwt { header, claims })
}

impl PublicKeyJwk {
    /// Verify the signature of the provided message using the JWK.
    ///
    /// # Errors
    ///
    /// Will return an error if the signature is invalid, the JWK is invalid, or the
    /// algorithm is unsupported.
    pub fn verify(&self, msg: &str, sig: &[u8]) -> anyhow::Result<()> {
        match self.crv {
            Curve::Es256K => self.verify_es256k(msg, sig),
            Curve::Ed25519 => self.verify_eddsa(msg, sig),
        }
    }

    // Verify the signature of the provided message using the ES256K algorithm.
    fn verify_es256k(&self, msg: &str, sig: &[u8]) -> anyhow::Result<()> {
        use ecdsa::{Signature, VerifyingKey};
        use k256::Secp256k1;

        // build verifying key
        let y = self.y.as_ref().ok_or_else(|| anyhow!("Proof JWT 'y' is invalid"))?;
        let mut sec1 = vec![0x04]; // uncompressed format
        sec1.append(&mut Base64UrlUnpadded::decode_vec(&self.x)?);
        sec1.append(&mut Base64UrlUnpadded::decode_vec(y)?);

        let verifying_key = VerifyingKey::<Secp256k1>::from_sec1_bytes(&sec1)?;
        let signature: Signature<Secp256k1> = Signature::from_slice(sig)?;
        let normalised = signature.normalize_s().unwrap_or(signature);

        Ok(verifying_key.verify(msg.as_bytes(), &normalised)?)
    }

    // Verify the signature of the provided message using the EdDSA algorithm.
    fn verify_eddsa(&self, msg: &str, sig_bytes: &[u8]) -> anyhow::Result<()> {
        use ed25519_dalek::{Signature, VerifyingKey};

        // build verifying key
        let x_bytes = Base64UrlUnpadded::decode_vec(&self.x)
            .map_err(|e| anyhow!("unable to base64 decode proof JWK 'x': {e}"))?;
        let bytes = &x_bytes.try_into().map_err(|_| anyhow!("invalid public key length"))?;
        let verifying_key = VerifyingKey::from_bytes(bytes)
            .map_err(|e| anyhow!("unable to build verifying key: {e}"))?;
        let signature = Signature::from_slice(sig_bytes)
            .map_err(|e| anyhow!("unable to build signature: {e}"))?;

        verifying_key
            .verify(msg.as_bytes(), &signature)
            .map_err(|e| anyhow!("unable to verify signature: {e}"))
    }
}

/// JWS definition.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Jws {
    /// The stringified CID of the DAG CBOR encoded message `descriptor` property.
    /// An empty string when JWS Unencoded Payload Option used.
    pub payload: String,

    /// JWS signatures.
    pub signatures: Vec<Signature>,
}

impl Jws {
    /// Verify JWS signatures.
    pub async fn verify<F, Fut>(&self, resolver: F) -> anyhow::Result<()>
    where
        F: Fn(String) -> Fut + Send + Sync,
        Fut: Future<Output = anyhow::Result<PublicKeyJwk>> + Send + Sync,
    {
        for signature in &self.signatures {
            let header = &signature.protected;
            let Some(kid) = header.kid() else {
                return Err(anyhow!("Missing key ID in JWS signature"));
            };

            // dereference `kid` to JWK matching key ID
            let public_jwk = resolver(kid.to_owned()).await?;

            let base64 = Base64UrlUnpadded::encode_string(&serde_json::to_vec(&header)?);
            let payload = format!("{base64}.{}", self.payload);
            let signature = Base64UrlUnpadded::decode_vec(&signature.signature)?;

            public_jwk.verify(&payload, &signature)?;
        }

        Ok(())
    }
}

/// An entry of the `signatures` array in a general JWS.
#[derive(Clone, Debug, Default)]
pub struct Signature {
    /// The base64 url-encoded JWS protected header when the JWS protected
    /// header is non-empty. Must have `alg` and `kid` properties set.
    pub protected: Protected,

    /// The base64 url-encoded JWS signature.
    pub signature: String,
}

impl Serialize for Signature {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::ser::Serializer,
    {
        // base64url encode header
        let bytes = serde_json::to_vec(&self.protected).map_err(serde::ser::Error::custom)?;
        let protected = Base64UrlUnpadded::encode_string(&bytes);

        // serialize the payload
        let mut state = serializer.serialize_struct("Signature", 2)?;
        state.serialize_field("protected", &protected)?;
        state.serialize_field("signature", &self.signature)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for Signature {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::de::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Inner {
            protected: String,
            signature: String,
        }

        let inner = Inner::deserialize(deserializer)?;
        let protected =
            Base64UrlUnpadded::decode_vec(&inner.protected).map_err(serde::de::Error::custom)?;
        let protected = serde_json::from_slice(&protected).map_err(serde::de::Error::custom)?;

        Ok(Signature {
            protected,
            signature: inner.signature,
        })
    }
}

/// JWS header.
///
/// N.B. The following headers are not included as they are unnecessary
/// for Vercre: `jku`, `x5u`, `x5t`, `x5t#S256`, `cty`, `crit`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Protected {
    /// Digital signature algorithm identifier as per IANA "JSON Web Signature
    /// and Encryption Algorithms" registry.
    pub alg: Algorithm,

    /// Used to declare the media type [IANA.MediaTypes] of the JWS.
    ///
    /// [IANA.MediaTypes]: (http://www.iana.org/assignments/media-types)
    pub typ: Type,

    /// The key material for the public key.
    #[serde(flatten)]
    pub key: KeyType,

    /// Contains a certificate (or certificate chain) corresponding to the key
    /// used to sign the JWT. This element MAY be used to convey a key
    /// attestation. In such a case, the actual key certificate will contain
    /// attributes related to the key properties.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x5c: Option<String>,

    /// Contains an OpenID.Federation Trust Chain. This element MAY be used to
    /// convey key attestation, metadata, metadata policies, federation
    /// Trust Marks and any other information related to a specific
    /// federation, if available in the chain.
    ///
    /// When used for signature verification, `kid` MUST be set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trust_chain: Option<String>,
}

impl Protected {
    /// Returns the `kid` if the key type is `KeyId`.
    pub fn kid(&self) -> Option<&str> {
        match &self.key {
            KeyType::KeyId(kid) => Some(kid),
            _ => None,
        }
    }

    /// Returns the `kid` if the key type is `KeyId`.
    pub fn jwk(&self) -> Option<&PublicKeyJwk> {
        match &self.key {
            KeyType::Jwk(jwk) => Some(jwk),
            _ => None,
        }
    }
}
