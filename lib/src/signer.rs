use ethers_core::{
    k256::{
        self,
        ecdsa::RecoveryId,
        elliptic_curve::{
            self,
            group::GroupEncoding,
            ops::Reduce,
            point::{AffineCoordinates, DecompressPoint},
            PrimeField,
        },
        AffinePoint, Secp256k1,
    },
    utils::hex,
};
use near_sdk::{ext_contract, near, AccountId, PromiseOrValue};
use thiserror::Error;

#[derive(Debug, Clone)]
#[near(serializers = [json, borsh])]
pub struct SignRequest {
    pub payload: [u8; 32],
    pub path: String,
    pub key_version: u32,
}

impl SignRequest {
    pub fn new(payload: [u8; 32], path: String, key_version: u32) -> Self {
        Self {
            payload,
            path,
            key_version,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[near(serializers = [json, borsh])]
pub struct SignResult {
    #[serde(rename = "big_r")]
    pub big_r_hex: String,
    #[serde(rename = "s")]
    pub s_hex: String,
}

#[near(serializers = [json])]
pub struct AffnPnt {
    pub affine_point: String,
}

#[near(serializers = [json])]
pub struct Sclr {
    pub scalar: String,
}

#[near(serializers = [json])]
pub struct SignatureResponse {
    pub big_r: AffnPnt,
    pub s: Sclr,
    pub recovery_id: u8,
}

#[allow(clippy::ptr_arg)]
#[ext_contract(ext_signer)]
pub trait SignerInterface {
    fn sign(&mut self, request: SignRequest) -> PromiseOrValue<SignatureResponse>;
    fn public_key(&self) -> near_sdk::PublicKey;
    fn derived_public_key(
        &self,
        path: String,
        predecessor: Option<AccountId>,
    ) -> near_sdk::PublicKey;
    fn latest_key_version(&self) -> u32;
}

impl SignResult {
    #[must_use]
    pub fn new(r: [u8; 32], s: [u8; 32], v: RecoveryId) -> Option<Self> {
        let big_r = Option::<AffinePoint>::from(AffinePoint::decompress(
            &r.into(),
            u8::from(v.is_y_odd()).into(),
        ))?;

        Some(Self {
            big_r_hex: hex::encode(big_r.to_bytes()),
            s_hex: hex::encode(s),
        })
    }

    #[must_use]
    pub fn from_ecdsa_signature(
        signature: ethers_core::k256::ecdsa::Signature,
        recovery_id: RecoveryId,
    ) -> Option<Self> {
        SignResult::new(
            signature.r().to_bytes().into(),
            signature.s().to_bytes().into(),
            recovery_id,
        )
    }
}

#[derive(Debug, Error)]
pub enum SignResultDecodeError {
    #[error("Failed to decode signature from hex: {0}")]
    Hex(#[from] hex::FromHexError),
    #[error("Invalid signature data")]
    InvalidSignatureData,
}

impl TryFrom<SignResult> for ethers_core::types::Signature {
    type Error = SignResultDecodeError;

    fn try_from(SignResult { big_r_hex, s_hex }: SignResult) -> Result<Self, Self::Error> {
        let big_r = Option::<AffinePoint>::from(AffinePoint::from_bytes(
            hex::decode(big_r_hex)?[..].into(),
        ))
        .ok_or(SignResultDecodeError::InvalidSignatureData)?;
        let s = hex::decode(s_hex)?;

        let r = <k256::Scalar as Reduce<<Secp256k1 as elliptic_curve::Curve>::Uint>>::reduce_bytes(
            &big_r.x(),
        );
        let x_is_reduced = r.to_repr() != big_r.x();

        let v = RecoveryId::new(big_r.y_is_odd().into(), x_is_reduced);

        Ok(ethers_core::types::Signature {
            r: r.to_bytes().as_slice().into(),
            s: s.as_slice().into(),
            v: v.to_byte().into(),
        })
    }
}

impl TryFrom<SignatureResponse> for ethers_core::types::Signature {
    type Error = SignResultDecodeError;

    fn try_from(signature_response: SignatureResponse) -> Result<Self, Self::Error> {
        let big_r = Option::<AffinePoint>::from(AffinePoint::from_bytes(
            hex::decode(signature_response.big_r.affine_point)?[..].into(),
        ))
        .ok_or(SignResultDecodeError::InvalidSignatureData)?;
        let s = hex::decode(signature_response.s.scalar)?;

        let r = <k256::Scalar as Reduce<<Secp256k1 as elliptic_curve::Curve>::Uint>>::reduce_bytes(
            &big_r.x(),
        );

        Ok(ethers_core::types::Signature {
            r: r.to_bytes().as_slice().into(),
            s: s.as_slice().into(),
            v: signature_response.recovery_id.into(),
        })
    }
}
