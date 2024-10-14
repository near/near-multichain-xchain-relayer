use lib::{
    chain_key::{ext_chain_key_token_approval_receiver, ChainKeyToken, ChainKeyTokenApproval},
    signer::{ext_signer, SignRequest, SignResult},
    Rejectable,
};
use near_sdk::{
    assert_one_yocto, collections::UnorderedMap, env, near, require, AccountId, AccountIdRef,
    BorshStorageKey, Gas, NearToken, PanicOnDefault, Promise, PromiseError, PromiseOrValue,
    PublicKey,
};
use near_sdk_contract_tools::hook::Hook;
#[allow(clippy::wildcard_imports)]
use near_sdk_contract_tools::nft::*;

/// OID for secp256k1 curve.
/// See: <https://oidref.com/1.3.132.0.10>
static SCHEME_OID: &str = "1.3.132.0.10";

#[derive(Debug, BorshStorageKey)]
#[near]
enum StorageKey {
    KeyData,
    ApprovalsFor(u32),
}

#[derive(Debug)]
#[near]
pub struct KeyData {
    pub approvals: UnorderedMap<AccountId, u32>,
    pub key_version: u32,
}

#[derive(Debug, PanicOnDefault, NonFungibleToken)]
#[non_fungible_token(transfer_hook = "Self", burn_hook = "Self")]
#[near(contract_state)]
pub struct NftKeyContract {
    pub next_id: u32,
    pub signer_contract_id: AccountId,
    pub key_data: UnorderedMap<u32, KeyData>,
}

fn generate_token_metadata(id: u32) -> TokenMetadata {
    TokenMetadata::new().title(format!("Chain Key Token #{id}"))
}

#[near]
impl NftKeyContract {
    #[private]
    #[init]
    pub fn new(signer_contract_id: AccountId) -> Self {
        let mut contract = Self {
            next_id: 0,
            signer_contract_id,
            key_data: UnorderedMap::new(StorageKey::KeyData),
        };

        contract.set_contract_metadata(&ContractMetadata::new("Chain Key Token", "CKT", None));

        contract
    }

    #[cfg(feature = "debug")]
    pub fn set_signer_contract_id(&mut self, account_id: AccountId) {
        self.signer_contract_id = account_id;
    }

    pub fn get_signer_contract_id(&self) -> &AccountIdRef {
        &self.signer_contract_id
    }

    fn generate_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1).unwrap_or_reject();
        id
    }

    pub fn mint(&mut self) -> Promise {
        let storage_usage_start = env::storage_usage();
        let id = self.generate_id();
        let predecessor = env::predecessor_account_id();
        self.storage_balance_of(predecessor.clone())
            .expect_or_reject("Predecessor has not registered for storage");

        ext_signer::ext(self.signer_contract_id.clone())
            .latest_key_version()
            .then(Self::ext(env::current_account_id()).mint_callback(
                id,
                predecessor,
                storage_usage_start,
            ))
    }

    #[private]
    pub fn mint_callback(
        &mut self,
        #[serializer(borsh)] id: u32,
        #[serializer(borsh)] predecessor: AccountId,
        #[serializer(borsh)] storage_usage_start: u64,
        #[callback_result] result: Result<u32, PromiseError>,
    ) -> u32 {
        let key_version = result.unwrap();

        self.key_data.insert(
            &id,
            &KeyData {
                key_version,
                approvals: UnorderedMap::new(StorageKey::ApprovalsFor(id)),
            },
        );
        self.mint_with_metadata(&id.to_string(), &predecessor, &generate_token_metadata(id))
            .unwrap_or_reject();

        self.storage_accounting(&predecessor, storage_usage_start)
            .unwrap_or_reject();

        id
    }
}

#[near]
impl ChainKeyToken for NftKeyContract {
    #[payable]
    fn ckt_sign_hash(
        &mut self,
        token_id: TokenId,
        path: Option<String>,
        payload: Vec<u8>,
        approval_id: Option<u32>,
    ) -> PromiseOrValue<String> {
        require!(
            env::attached_deposit() >= NearToken::from_yoctonear(1),
            "Requires attached deposit of at least 1 yoctoNEAR"
        );

        let id = token_id.parse().expect_or_reject("Invalid token ID");
        let path = path.unwrap_or_default();

        let expected_owner_id = env::predecessor_account_id();
        let actual_owner_id = self.token_owner(&token_id.to_string());

        let key_data = self
            .key_data
            .get(&id)
            .expect_or_reject("Missing data for key");

        require!(
            Some(&expected_owner_id) == actual_owner_id.as_ref()
                || key_data
                    .approvals
                    .get(&env::predecessor_account_id())
                    .zip(approval_id)
                    .map_or(false, |(actual, expected)| actual == expected),
            "Unauthorized",
        );

        PromiseOrValue::Promise(
            ext_signer::ext(self.signer_contract_id.clone())
                .with_attached_deposit(env::attached_deposit())
                .sign(SignRequest::new(
                    payload.try_into().unwrap(),
                    make_path_string(&token_id, &path),
                    key_data.key_version,
                ))
                .then(
                    Self::ext(env::current_account_id())
                        .with_static_gas(Self::SIGN_CALLBACK_GAS)
                        .with_unused_gas_weight(0)
                        .sign_callback(),
                ),
        )
    }

    fn ckt_public_key_for(
        &mut self,
        token_id: TokenId,
        path: Option<String>,
    ) -> PromiseOrValue<PublicKey> {
        let path = path.unwrap_or_default();

        PromiseOrValue::Promise(
            ext_signer::ext(self.signer_contract_id.clone())
                .derived_public_key(make_path_string(&token_id, &path), None),
        )
    }

    fn ckt_scheme_oid(&self) -> String {
        SCHEME_OID.to_string()
    }
}

fn make_path_string(token_id: &str, path: &str) -> String {
    format!("{token_id},{path}")
}

#[near]
impl NftKeyContract {
    const SIGN_CALLBACK_GAS: Gas = Gas::from_tgas(3);

    #[private]
    #[must_use]
    pub fn sign_callback(
        &self,
        #[callback_result] result: Result<SignResult, PromiseError>,
    ) -> String {
        let mpc_signature = result.unwrap();
        let ethers_signature: ethers_core::types::Signature =
            mpc_signature.try_into().unwrap_or_reject();
        ethers_signature.to_string()
    }

    #[private]
    pub fn ckt_approve_callback(
        &mut self,
        #[serializer(borsh)] token_id: u32,
        #[serializer(borsh)] account_id: AccountId,
        #[serializer(borsh)] approval_id: u32,
        #[callback_result] result: Result<bool, PromiseError>,
    ) -> Option<u32> {
        if result == Ok(false) {
            Some(approval_id)
        } else {
            let mut key_data = self.key_data.get(&token_id).unwrap_or_reject();
            let ejected_id = key_data.approvals.remove(&account_id);
            self.key_data.insert(&token_id, &key_data);
            require!(ejected_id == Some(approval_id), "Inconsistent approval ID");
            None
        }
    }

    #[private]
    pub fn ckt_revoke_callback(&self) {}

    fn require_is_token_owner(&self, predecessor: &AccountId, token_id: &TokenId) {
        let actual_owner = Nep171Controller::token_owner(self, token_id);
        require!(actual_owner.as_ref() == Some(predecessor), "Unauthorized");
    }

    fn approve(&mut self, token_id: u32, account_id: &AccountId) -> u32 {
        let approval_id = self.generate_id();

        let mut key_data = self
            .key_data
            .get(&token_id)
            .expect_or_reject("Missing data for key");
        key_data.approvals.insert(account_id, &approval_id);
        self.key_data.insert(&token_id, &key_data);

        approval_id
    }

    fn revoke(&mut self, token_id: u32, account_id: &AccountId) -> Option<u32> {
        self.key_data.get(&token_id).and_then(|mut key_data| {
            let removed = key_data.approvals.remove(account_id);
            self.key_data.insert(&token_id, &key_data);
            removed
        })
    }
}

#[near]
impl ChainKeyTokenApproval for NftKeyContract {
    #[payable]
    fn ckt_approve(&mut self, token_id: TokenId, account_id: AccountId) -> u32 {
        assert_one_yocto();
        let predecessor = env::predecessor_account_id();
        self.require_is_token_owner(&predecessor, &token_id);
        let id = token_id.parse().expect_or_reject("Invalid token ID");
        self.approve(id, &account_id)
    }

    #[payable]
    fn ckt_approve_call(
        &mut self,
        token_id: String,
        account_id: AccountId,
        msg: Option<String>,
    ) -> PromiseOrValue<Option<u32>> {
        assert_one_yocto();
        let predecessor = env::predecessor_account_id();
        self.require_is_token_owner(&predecessor, &token_id);
        let id = token_id.parse().expect_or_reject("Invalid token ID");
        let approval_id = self.approve(id, &account_id);

        PromiseOrValue::Promise(
            ext_chain_key_token_approval_receiver::ext(account_id.clone())
                .ckt_on_approved(predecessor, token_id, approval_id, msg.unwrap_or_default())
                .then(Self::ext(env::current_account_id()).ckt_approve_callback(
                    id,
                    account_id,
                    approval_id,
                )),
        )
    }

    #[payable]
    fn ckt_revoke(&mut self, token_id: TokenId, account_id: AccountId) {
        assert_one_yocto();
        let predecessor = env::predecessor_account_id();
        self.require_is_token_owner(&predecessor, &token_id);
        let id = token_id.parse().expect_or_reject("Invalid token ID");
        self.revoke(id, &account_id);
    }

    #[payable]
    fn ckt_revoke_call(
        &mut self,
        token_id: String,
        account_id: AccountId,
        msg: Option<String>,
    ) -> PromiseOrValue<()> {
        assert_one_yocto();
        let predecessor = env::predecessor_account_id();
        self.require_is_token_owner(&predecessor, &token_id);
        let id = token_id.parse().expect_or_reject("Invalid token ID");
        let revoked_approval_id = self.revoke(id, &account_id);

        if let Some(revoked_approval_id) = revoked_approval_id {
            PromiseOrValue::Promise(
                ext_chain_key_token_approval_receiver::ext(account_id)
                    .ckt_on_revoked(
                        predecessor,
                        token_id,
                        revoked_approval_id,
                        msg.unwrap_or_default(),
                    )
                    .then(Self::ext(env::current_account_id()).ckt_revoke_callback()),
            )
        } else {
            PromiseOrValue::Value(())
        }
    }

    #[payable]
    fn ckt_revoke_all(&mut self, token_id: TokenId) -> near_sdk::json_types::U64 {
        assert_one_yocto();
        let predecessor = env::predecessor_account_id();
        self.require_is_token_owner(&predecessor, &token_id);

        let id: u32 = token_id.parse().expect_or_reject("Invalid token ID");
        let Some(mut key_data) = self.key_data.get(&id) else {
            return 0.into();
        };

        let len = key_data.approvals.len();
        key_data.approvals.clear();
        self.key_data.insert(&id, &key_data);

        len.into()
    }

    fn ckt_approval_id_for(&self, token_id: TokenId, account_id: AccountId) -> Option<u32> {
        let id: u32 = token_id.parse().expect_or_reject("Invalid token ID");

        self.key_data
            .get(&id)
            .and_then(|key_data| key_data.approvals.get(&account_id))
    }
}

impl Hook<NftKeyContract, Nep171Transfer<'_>> for NftKeyContract {
    fn hook<R>(
        contract: &mut NftKeyContract,
        transfer: &Nep171Transfer<'_>,
        f: impl FnOnce(&mut NftKeyContract) -> R,
    ) -> R {
        contract.ckt_revoke_all(transfer.token_id.clone());
        f(contract)
    }
}

impl Hook<NftKeyContract, Nep171Burn<'_>> for NftKeyContract {
    fn hook<R>(
        contract: &mut NftKeyContract,
        burn: &Nep171Burn,
        f: impl FnOnce(&mut NftKeyContract) -> R,
    ) -> R {
        for token_id in &burn.token_ids {
            contract.ckt_revoke_all(token_id.clone());
        }
        f(contract)
    }
}
