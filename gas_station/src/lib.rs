use ethers_core::{
    types::{transaction::eip2718::TypedTransaction, U256},
    utils::hex,
};
use lib::{
    asset::{AssetBalance, AssetId},
    chain_key::ext_chain_key_token,
    foreign_address::ForeignAddress,
    pyth::{self, ext_pyth},
    Rejectable,
};
use near_sdk::{
    collections::{UnorderedMap, UnorderedSet, Vector},
    env,
    json_types::{U128, U64},
    near, near_bindgen, require, AccountId, BorshStorageKey, Gas, PanicOnDefault, Promise,
    PromiseError, PromiseOrValue,
};
#[allow(clippy::wildcard_imports)]
use near_sdk_contract_tools::pause::*;
use near_sdk_contract_tools::{rbac::Rbac, standard::nep297::Event, Pause, Rbac};

pub mod chain_configuration;
use chain_configuration::ForeignChainConfiguration;

pub mod contract_event;
use contract_event::{ContractEvent, TransactionSequenceCreated, TransactionSequenceSigned};

mod error;
#[allow(clippy::wildcard_imports)]
use error::*;

mod impl_chain_key_nft;
pub use impl_chain_key_nft::ChainKeyReceiverMsg;
#[cfg(feature = "debug")]
mod impl_debug;
mod impl_management;
mod impl_nep141_receiver;

pub mod signature_request;
use signature_request::{SignatureRequest, Status};

mod utils;
use utils::{decode_transaction_request, sighash_for_mpc_signing};

pub mod valid_transaction_request;
use valid_transaction_request::ValidTransactionRequest;

const DEFAULT_EXPIRE_SEQUENCE_AFTER_BLOCKS: u64 = 5 * 60; // 5ish minutes at 1s/block

#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[near(serializers = [borsh, json])]
pub struct Flags {
    pub is_sender_whitelist_enabled: bool,
    pub is_receiver_whitelist_enabled: bool,
}

#[near(serializers = [json])]
pub struct GetForeignChain {
    pub chain_id: U64,
    pub oracle_asset_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[near(serializers = [borsh, json])]
pub struct PendingTransactionSequence {
    pub created_by_account_id: AccountId,
    pub signature_requests: Vec<SignatureRequest>,
    pub created_at_block_height: U64,
    pub escrow: Option<AssetBalance>,
}

impl PendingTransactionSequence {
    pub fn all_signed(&self) -> bool {
        self.signature_requests
            .iter()
            .all(SignatureRequest::is_signed)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[near]
pub struct TransactionSequenceSignedEventAt {
    pub block_height: u64,
    pub event: contract_event::TransactionSequenceSigned,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[near(serializers = [json])]
pub struct Nep141ReceiverCreateTransactionArgs {
    pub token_id: String,
    pub transaction_rlp_hex: String,
    pub use_paymaster: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[near(serializers = [borsh, json])]
pub struct TransactionSequenceCreation {
    pub id: U64,
    pub pending_signature_count: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[near(serializers = [borsh, json])]
pub struct ChainKeyData {
    pub public_key_bytes: Vec<u8>,
    pub authorization: ChainKeyAuthorization,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[near(serializers = [borsh, json])]
pub enum ChainKeyAuthorization {
    Owned,
    Approved(u32),
}

impl ChainKeyAuthorization {
    pub fn is_owned(&self) -> bool {
        matches!(self, Self::Owned)
    }

    pub fn is_approved(&self) -> bool {
        matches!(self, Self::Approved(..))
    }

    pub fn is_approved_with_id(&self, approval_id: u32) -> bool {
        self == &Self::Approved(approval_id)
    }

    pub fn to_approval_id(&self) -> Option<u32> {
        if let Self::Approved(approval_id) = self {
            Some(*approval_id)
        } else {
            None
        }
    }
}

#[derive(BorshStorageKey, Hash, Clone, Debug, PartialEq, Eq)]
#[near]
pub enum StorageKey {
    SenderWhitelist,
    ReceiverWhitelist,
    ForeignChains,
    Paymasters(u64),
    PendingTransactionSequences,
    CollectedFees,
    SignedTransactionSequences,
    AcceptedLocalAssets,
    UserChainKeys,
    UserChainKeysFor(AccountId),
    PaymasterKeys,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, BorshStorageKey)]
#[near]
pub enum Role {
    Administrator,
    MarketMaker,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[near(serializers = [borsh, json])]
pub struct LocalAssetConfiguration {
    pub oracle_asset_id: [u8; 32],
    pub decimals: u8,
}

#[derive(PanicOnDefault, Debug, Pause, Rbac)]
#[rbac(roles = "Role")]
#[near(contract_state)]
pub struct Contract {
    pub next_unique_id: u64,
    pub signer_contract_id: AccountId,
    pub oracle_id: AccountId,
    pub accepted_local_assets: UnorderedMap<AssetId, LocalAssetConfiguration>,
    pub flags: Flags,
    pub expire_sequence_after_blocks: u64,
    pub foreign_chains: UnorderedMap<u64, ForeignChainConfiguration>,
    pub user_chain_keys: UnorderedMap<AccountId, UnorderedMap<String, ChainKeyData>>,
    pub paymaster_keys: UnorderedMap<String, ChainKeyData>,
    pub sender_whitelist: UnorderedSet<AccountId>,
    pub receiver_whitelist: UnorderedSet<ForeignAddress>,
    pub pending_transaction_sequences: UnorderedMap<u64, PendingTransactionSequence>,
    /// TODO: Hopefully temporary measure to eliminate the need for an indexer.
    pub signed_transaction_sequences: Vector<TransactionSequenceSignedEventAt>,
    pub collected_fees: UnorderedMap<AssetId, U128>,
}

#[near_bindgen]
impl Contract {
    #[private]
    #[init]
    pub fn new(
        signer_contract_id: AccountId,
        oracle_id: AccountId,
        expire_sequence_after_blocks: Option<U64>,
    ) -> Self {
        let mut contract = Self {
            next_unique_id: 0,
            signer_contract_id,
            oracle_id,
            accepted_local_assets: UnorderedMap::new(StorageKey::AcceptedLocalAssets),
            flags: Flags::default(),
            expire_sequence_after_blocks: expire_sequence_after_blocks
                .map_or(DEFAULT_EXPIRE_SEQUENCE_AFTER_BLOCKS, u64::from),
            foreign_chains: UnorderedMap::new(StorageKey::ForeignChains),
            user_chain_keys: UnorderedMap::new(StorageKey::UserChainKeys),
            paymaster_keys: UnorderedMap::new(StorageKey::PaymasterKeys),
            sender_whitelist: UnorderedSet::new(StorageKey::SenderWhitelist),
            receiver_whitelist: UnorderedSet::new(StorageKey::ReceiverWhitelist),
            pending_transaction_sequences: UnorderedMap::new(
                StorageKey::PendingTransactionSequences,
            ),
            signed_transaction_sequences: Vector::new(StorageKey::SignedTransactionSequences),
            collected_fees: UnorderedMap::new(StorageKey::CollectedFees),
        };

        Rbac::add_role(
            &mut contract,
            &env::predecessor_account_id(),
            &Role::Administrator,
        );

        contract
    }

    // Public methods

    #[payable]
    pub fn create_transaction(
        &mut self,
        token_id: String,
        transaction_rlp_hex: String,
        use_paymaster: Option<bool>,
    ) -> PromiseOrValue<TransactionSequenceCreation> {
        self.create_transaction_inner(
            token_id,
            env::predecessor_account_id(),
            transaction_rlp_hex,
            use_paymaster,
            AssetBalance::native(env::attached_deposit().as_yoctonear()),
        )
    }

    fn create_transaction_inner(
        &mut self,
        token_id: String,
        account_id: AccountId,
        transaction_rlp_hex: String,
        use_paymaster: Option<bool>,
        deposit: AssetBalance,
    ) -> PromiseOrValue<TransactionSequenceCreation> {
        <Self as Pause>::require_unpaused();

        let transaction =
            ValidTransactionRequest::try_from(decode_transaction_request(&transaction_rlp_hex))
                .unwrap_or_reject();

        // Whitelisting
        self.filter_transaction(&account_id, &transaction);

        // Assert predecessor can use requested key path
        let user_chain_keys = self
            .user_chain_keys
            .get(&account_id)
            .expect_or_reject("No managed keys for predecessor");

        let user_chain_key = user_chain_keys
            .get(&token_id)
            .expect_or_reject("Predecessor unauthorized for the requested chain key token ID");

        let use_paymaster = use_paymaster.unwrap_or(false);

        if use_paymaster {
            require!(deposit.amount.0 > 0, "Deposit is required to pay for gas");

            let accepted_local_asset = self
                .accepted_local_assets
                .get(&deposit.asset_id)
                .expect_or_reject("Unsupported deposit asset");

            let chain_id = transaction.chain_id();
            let foreign_chain_configuration = self.get_chain(chain_id.as_u64()).unwrap_or_reject();

            ext_pyth::ext(self.oracle_id.clone())
                .get_ema_price(pyth::PriceIdentifier(accepted_local_asset.oracle_asset_id))
                .and(
                    ext_pyth::ext(self.oracle_id.clone()).get_ema_price(pyth::PriceIdentifier(
                        foreign_chain_configuration.oracle_asset_id,
                    )),
                )
                .then(
                    Self::ext(env::current_account_id()).create_transaction_callback(
                        account_id,
                        token_id,
                        deposit,
                        transaction,
                    ),
                )
                .into()
        } else {
            let chain_id = transaction.chain_id;

            let pending_transaction_sequence = PendingTransactionSequence {
                signature_requests: vec![SignatureRequest::new(
                    &token_id,
                    user_chain_key.authorization,
                    transaction,
                    false,
                )],
                created_by_account_id: account_id,
                created_at_block_height: env::block_height().into(),
                escrow: None,
            };

            let creation = self.insert_transaction_sequence(&pending_transaction_sequence);

            ContractEvent::TransactionSequenceCreated(TransactionSequenceCreated {
                id: creation.id,
                foreign_chain_id: chain_id.to_string(),
                pending_transaction_sequence,
            })
            .emit();

            PromiseOrValue::Value(creation)
        }
    }

    fn try_create_transaction_callback(
        &mut self,
        sender: &AccountId,
        token_id: String,
        deposit: &AssetBalance,
        transaction_request: ValidTransactionRequest,
        local_asset_price_result: Result<pyth::Price, PromiseError>,
        foreign_asset_price_result: Result<pyth::Price, PromiseError>,
    ) -> Result<(u128, TransactionSequenceCreation), TryCreateTransactionCallbackError> {
        let local_asset_price = local_asset_price_result.map_err(|_| OracleQueryFailureError)?;
        let foreign_asset_price =
            foreign_asset_price_result.map_err(|_| OracleQueryFailureError)?;

        let accepted_local_asset = self
            .accepted_local_assets
            .get(&deposit.asset_id)
            .unwrap_or_reject();

        let user_chain_key = self
            .user_chain_keys
            .get(sender)
            .and_then(|user_chain_keys| user_chain_keys.get(&token_id))
            .ok_or_else(|| SenderUnauthorizedForNftChainKeyError {
                sender: sender.clone(),
                token_id: token_id.clone(),
            })?;

        let sender_foreign_address =
            ForeignAddress::from_raw_public_key(&user_chain_key.public_key_bytes);

        let mut foreign_chain = self
            .foreign_chains
            .get(&transaction_request.chain_id)
            .ok_or(ChainConfigurationDoesNotExistError {
                chain_id: transaction_request.chain_id,
            })?;

        let gas_tokens_to_sponsor_transaction =
            foreign_chain.calculate_gas_tokens_to_sponsor_transaction(&transaction_request)?;

        let local_asset_fee = foreign_chain.price_for_gas_tokens(
            gas_tokens_to_sponsor_transaction,
            &foreign_asset_price,
            &local_asset_price,
            accepted_local_asset.decimals,
        )?;

        let refund = deposit.amount.0.checked_sub(local_asset_fee).ok_or(
            InsufficientDepositForFeeError {
                deposit: deposit.amount.0,
                fee: local_asset_fee,
            },
        )?;

        let paymaster_signature_request = self.create_funding_signature_request(
            &mut foreign_chain,
            &transaction_request,
            sender_foreign_address,
            gas_tokens_to_sponsor_transaction,
        )?;

        self.foreign_chains
            .insert(&transaction_request.chain_id, &foreign_chain);

        // After this point, the function should be virtually infallible, excluding out-of-gas errors.

        let signature_requests = vec![
            paymaster_signature_request,
            SignatureRequest::new(
                &token_id,
                user_chain_key.authorization,
                transaction_request.clone(),
                false,
            ),
        ];

        let pending_transaction_sequence = PendingTransactionSequence {
            signature_requests,
            created_by_account_id: sender.clone(),
            created_at_block_height: env::block_height().into(),
            escrow: Some(AssetBalance {
                amount: local_asset_fee.into(),
                asset_id: deposit.asset_id.clone(),
            }),
        };

        let creation = self.insert_transaction_sequence(&pending_transaction_sequence);

        ContractEvent::TransactionSequenceCreated(TransactionSequenceCreated {
            id: creation.id,
            foreign_chain_id: transaction_request.chain_id.to_string(),
            pending_transaction_sequence,
        })
        .emit();

        Ok((refund, creation))
    }

    #[private]
    pub fn create_transaction_callback(
        &mut self,
        #[serializer(borsh)] sender: AccountId,
        #[serializer(borsh)] token_id: String,
        #[serializer(borsh)] deposit: AssetBalance,
        #[serializer(borsh)] transaction_request: ValidTransactionRequest,
        #[callback_result] local_asset_price_result: Result<pyth::Price, PromiseError>,
        #[callback_result] foreign_asset_price_result: Result<pyth::Price, PromiseError>,
    ) -> PromiseOrValue<TransactionSequenceCreation> {
        let (refund, creation) = match self.try_create_transaction_callback(
            &sender,
            token_id,
            &deposit,
            transaction_request,
            local_asset_price_result,
            foreign_asset_price_result,
        ) {
            Ok((refund, creation)) => (refund, creation),
            Err(e) => {
                // Failure: return deposit.
                return PromiseOrValue::Promise(
                    deposit
                        .asset_id
                        .transfer(sender, deposit.amount)
                        .then(Self::ext(env::current_account_id()).throw(e.to_string())),
                );
            }
        };

        if refund > 0 {
            // Refund excess
            deposit.asset_id.transfer(sender, refund);
        }

        PromiseOrValue::Value(creation)
    }

    #[private]
    pub fn throw(&mut self, #[serializer(borsh)] error_str: String) {
        env::panic_str(&error_str);
    }

    #[payable]
    pub fn sign_next(&mut self, id: U64) -> Promise {
        <Self as Pause>::require_unpaused();

        let id = id.0;

        let mut transaction = self
            .pending_transaction_sequences
            .get(&id)
            .expect_or_reject(TransactionSequenceDoesNotExistError {
                transaction_sequence_id: id,
            });

        // ensure not expired
        require!(
            env::block_height().saturating_sub(transaction.created_at_block_height.0)
                <= self.expire_sequence_after_blocks,
            "Transaction is expired",
        );

        // ensure only signed by original creator
        require!(
            transaction.created_by_account_id == env::predecessor_account_id(),
            "Predecessor must be the transaction creator",
        );

        let (index, next_signature_request) = transaction
            .signature_requests
            .iter_mut()
            .enumerate()
            .find(|(_, r)| r.is_pending())
            .expect_or_reject("No pending or non-in-flight signature requests");

        next_signature_request.status = Status::InFlight;

        #[allow(clippy::cast_possible_truncation)]
        let ret = ext_chain_key_token::ext(self.signer_contract_id.clone())
            .with_attached_deposit(env::attached_deposit())
            .ckt_sign_hash(
                next_signature_request.token_id.clone(),
                None,
                sighash_for_mpc_signing(next_signature_request.transaction.clone()).to_vec(),
                next_signature_request.authorization.to_approval_id(),
            )
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(Self::SIGN_NEXT_CALLBACK_GAS)
                    .with_unused_gas_weight(0)
                    .sign_next_callback(id.into(), index as u32),
            );

        self.pending_transaction_sequences.insert(&id, &transaction);

        ret
    }

    const SIGN_NEXT_CALLBACK_GAS: Gas = Gas::from_tgas(3);

    #[private]
    pub fn sign_next_callback(
        &mut self,
        id: U64,
        index: u32,
        #[callback_result] result: Result<String, PromiseError>,
    ) -> String {
        let id = id.0;

        let mut pending_transaction_sequence = self
            .pending_transaction_sequences
            .get(&id)
            .expect_or_reject(TransactionSequenceDoesNotExistError {
                transaction_sequence_id: id,
            });

        let request = pending_transaction_sequence
            .signature_requests
            .get_mut(index as usize)
            .expect_or_reject(SignatureRequestDoesNoteExistError {
                transaction_sequence_id: id,
                index,
            });

        if !request.is_in_flight() {
            env::panic_str(&format!(
                "Inconsistent state: Signature request {id}.{index} should be in-flight but is not"
            ));
        }

        // TODO: Fraud proofs.
        let signature = result
            .ok()
            .expect_or_reject("Failed to produce signature")
            .parse()
            .unwrap_or_reject();

        let transaction: TypedTransaction = request.transaction.clone().into();

        let rlp_signed = transaction.rlp_signed(&signature);

        request.set_signature(signature);

        // Remove escrow from record.
        // This is important to ensuring that refund logic works correctly.
        if let Some(escrow) = pending_transaction_sequence.escrow.take() {
            let mut collected_fees = self.collected_fees.get(&escrow.asset_id).unwrap_or(U128(0));
            // This should not fail, but if it does fail, that means the token
            // in question incorrectly implements the NEP-141 standard, which
            // dictates that the total supply fits in 128 bits.
            collected_fees.0 = collected_fees
                .0
                .checked_add(escrow.amount.0)
                .unwrap_or_reject();
            self.collected_fees
                .insert(&escrow.asset_id, &collected_fees);
        }

        let chain_id = request.transaction.chain_id;

        let all_signatures = pending_transaction_sequence
            .signature_requests
            .iter()
            .try_fold(vec![], |mut v, r| {
                if let Status::Signed { signature } = &r.status {
                    v.push((r.transaction.clone(), signature.clone()));
                    Some(v)
                } else {
                    None
                }
            });

        self.pending_transaction_sequences
            .insert(&id, &pending_transaction_sequence);

        if let Some(all_signatures) = all_signatures {
            let e = TransactionSequenceSigned {
                id: id.into(),
                foreign_chain_id: chain_id.to_string(),
                created_by_account_id: pending_transaction_sequence.created_by_account_id.clone(),
                signed_transactions: all_signatures
                    .into_iter()
                    .map(|(t, s)| {
                        hex::encode_prefixed(t.into_typed_transaction().rlp_signed(&s.into()))
                    })
                    .collect(),
            };

            self.signed_transaction_sequences
                .push(&TransactionSequenceSignedEventAt {
                    block_height: env::block_height(),
                    event: e.clone(),
                });

            ContractEvent::TransactionSequenceSigned(e).emit();

            // Remove transaction if all requests have been signed
            self.pending_transaction_sequences.remove(&id);
        }

        hex::encode_prefixed(&rlp_signed)
    }

    pub fn remove_transaction(&mut self, id: U64) -> PromiseOrValue<()> {
        <Self as Pause>::require_unpaused();

        let transaction = self
            .pending_transaction_sequences
            .get(&id.0)
            .expect_or_reject(TransactionSequenceDoesNotExistError {
                transaction_sequence_id: id.0,
            });

        require!(
            transaction.created_by_account_id == env::predecessor_account_id(),
            "Unauthorized",
        );

        for signature_request in &transaction.signature_requests {
            require!(
                !signature_request.is_in_flight(),
                "Signature request is in-flight and cannot be removed",
            );
        }

        let ret = transaction
            .escrow
            .as_ref()
            .map_or(PromiseOrValue::Value(()), |escrow| {
                PromiseOrValue::Promise(
                    escrow
                        .asset_id
                        .transfer(transaction.created_by_account_id.clone(), escrow.amount),
                )
            });

        self.pending_transaction_sequences.remove(&id.0);

        ret
    }
}

impl Contract {
    #[allow(clippy::unused_self)]
    fn require_unpaused_or_administrator(&self, account_id: &AccountId) {
        if !<Self as Rbac>::has_role(account_id, &Role::Administrator) {
            <Self as Pause>::require_unpaused();
        }
    }

    fn with_mut_chain<R>(
        &mut self,
        chain_id: u64,
        f: impl FnOnce(&mut ForeignChainConfiguration) -> R,
    ) -> R {
        let mut config = self
            .foreign_chains
            .get(&chain_id)
            .expect_or_reject(ChainConfigurationDoesNotExistError { chain_id });
        let ret = f(&mut config);
        self.foreign_chains.insert(&chain_id, &config);
        ret
    }

    fn get_chain(
        &self,
        chain_id: u64,
    ) -> Result<ForeignChainConfiguration, ChainConfigurationDoesNotExistError> {
        self.foreign_chains
            .get(&chain_id)
            .ok_or(ChainConfigurationDoesNotExistError { chain_id })
    }

    fn generate_unique_id(&mut self) -> u64 {
        let id = self.next_unique_id;
        self.next_unique_id = self
            .next_unique_id
            .checked_add(1)
            .expect_or_reject("Failed to generate unique ID");
        id
    }

    fn filter_transaction(&self, sender_id: &AccountId, transaction: &ValidTransactionRequest) {
        // Check receiver whitelist
        if self.flags.is_receiver_whitelist_enabled {
            require!(
                self.receiver_whitelist.contains(&transaction.to),
                "Receiver is not whitelisted",
            );
        }

        // Check sender whitelist
        if self.flags.is_sender_whitelist_enabled {
            require!(
                self.sender_whitelist.contains(sender_id),
                "Sender is not whitelisted",
            );
        }
    }

    /// Create a paymaster funding transaction that provides funding for the
    /// maximum amount of gas required by the transaction.
    ///
    /// # Errors
    ///
    /// - If the foreign chain ID is not supported.
    /// - If there is not a paymaster configured for the foreign chain.
    /// - If the price data provided is invalid.
    /// - If the paymaster does not have enough available balance.
    pub fn create_funding_signature_request(
        &self,
        foreign_chain: &mut ForeignChainConfiguration,
        transaction: &ValidTransactionRequest,
        sender_foreign_address: ForeignAddress,
        gas_tokens_to_sponsor_transaction: U256,
    ) -> Result<SignatureRequest, RequestNonceError> {
        foreign_chain.with_request_nonce(
            gas_tokens_to_sponsor_transaction,
            |foreign_chain, paymaster| {
                let paymaster_transaction = ValidTransactionRequest {
                    chain_id: transaction.chain_id,
                    to: sender_foreign_address,
                    value: gas_tokens_to_sponsor_transaction.0,
                    gas: foreign_chain.transfer_gas,
                    data: vec![],
                    nonce: U256::from(paymaster.nonce).0,
                    access_list_rlp: vec![0xc0 /* rlp encoding for empty list */],
                    max_priority_fee_per_gas: transaction.max_priority_fee_per_gas,
                    max_fee_per_gas: transaction.max_fee_per_gas,
                };

                let paymaster_authorization = self
                    .paymaster_keys
                    .get(&paymaster.token_id)
                    .unwrap_or_reject() // inconsistent state if this fails
                    .authorization;

                SignatureRequest::new(
                    &paymaster.token_id,
                    paymaster_authorization,
                    paymaster_transaction,
                    true,
                )
            },
        )
    }

    fn insert_transaction_sequence(
        &mut self,
        pending_transaction: &PendingTransactionSequence,
    ) -> TransactionSequenceCreation {
        #[allow(clippy::cast_possible_truncation)]
        let pending_signature_count = pending_transaction.signature_requests.len() as u32;

        let id = self.generate_unique_id();

        self.pending_transaction_sequences
            .insert(&id, pending_transaction);

        TransactionSequenceCreation {
            id: id.into(),
            pending_signature_count,
        }
    }
}
