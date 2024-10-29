use lib::{
    kdf::sha256,
    signer::{AffnPnt, Sclr, SignRequest, SignResult, SignatureResponse, SignerInterface},
    Rejectable,
};
use near_sdk::{env, near, require, AccountId, PromiseOrValue, PublicKey};

#[must_use]
pub fn construct_spoof_key(
    predecessor: &[u8],
    path: &[u8],
) -> ethers_core::k256::ecdsa::SigningKey {
    let predecessor_hash = sha256([predecessor, b",", path].concat().as_slice());
    ethers_core::k256::ecdsa::SigningKey::from_bytes(predecessor_hash.as_slice().into()).unwrap()
}

const KEY_VERSION: u32 = 0;

#[derive(Default, Debug)]
#[near(contract_state)]
pub struct MockSignerContract {}

#[near]
impl SignerInterface for MockSignerContract {
    #[payable]
    fn sign(&mut self, request: SignRequest) -> PromiseOrValue<SignatureResponse> {
        require!(
            request.key_version == KEY_VERSION,
            "Key version not supported",
        );

        let predecessor = env::predecessor_account_id();
        // This is unused, but needs to be in the sign signature.
        let signing_key = construct_spoof_key(predecessor.as_bytes(), request.path.as_bytes());
        let (sig, recid) = signing_key
            .sign_prehash_recoverable(&request.payload)
            .unwrap();

        // Create SignResult from the signature
        let sign_result = SignResult::from_ecdsa_signature(sig, recid).unwrap();

        // Convert SignResult to SignatureResponse
        let signature_response = SignatureResponse {
            big_r: AffnPnt {
                affine_point: sign_result.big_r_hex,
            },
            s: Sclr {
                scalar: sign_result.s_hex,
            },
            recovery_id: recid.to_byte(),
        };

        PromiseOrValue::Value(signature_response)
    }

    fn public_key(&self) -> PublicKey {
        "secp256k1:37aFybhUHCxRdDkuCcB3yHzxqK7N8EQ745MujyAQohXSsYymVeHzhLxKvZ2qYeRHf3pGFiAsxqFJZjpF9gP2JV5u"
            .parse()
            .unwrap()
    }

    fn derived_public_key(&self, path: String, predecessor: Option<AccountId>) -> PublicKey {
        let predecessor = predecessor.unwrap_or_else(env::predecessor_account_id);
        let signing_key = construct_spoof_key(predecessor.as_bytes(), path.as_bytes());
        let verifying_key = signing_key.verifying_key();
        let encoded = verifying_key.to_encoded_point(false);

        PublicKey::from_parts(
            near_sdk::CurveType::SECP256K1,
            encoded.to_bytes()[1..].to_vec(),
        )
        .unwrap_or_reject()
    }

    fn latest_key_version(&self) -> u32 {
        KEY_VERSION
    }
}
