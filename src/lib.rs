use ethers::{
    types::{
        transaction::eip2718::TypedTransaction, NameOrAddress, TransactionRequest, H160, U256,
    },
    utils::rlp::{Decodable, Rlp},
};
use near_sdk::{
    borsh::{self, BorshDeserialize, BorshSerialize},
    env,
    json_types::U64,
    near_bindgen, require,
    serde::{Deserialize, Serialize},
    store::{UnorderedMap, UnorderedSet},
    AccountId, BorshStorageKey, PanicOnDefault, Promise, PromiseError,
};
use near_sdk_contract_tools::{event, owner::*, standard::nep297::Event, Owner};

mod oracle;
use oracle::{ext_oracle, AssetOptionalPrice, PriceData};

mod signer_contract;
use signer_contract::{ext_signer, MpcSignature};

mod signature_request;
use signature_request::{SignatureRequest, SignatureRequestStatus};

mod utils;
use utils::*;

mod xchain_address;
use xchain_address::XChainAddress;

type XChainTokenAmount = ethers::types::U256;

/// A successful request will emit two events, one for the request and one for
/// the finalized transaction, in that order. The `id` field will be the same
/// for both events.
///
/// IDs are arbitrarily chosen by the contract. An ID is guaranteed to be unique
/// within the contract.
// #[event(version = "0.1.0", standard = "x-multichain-sig")]
// pub enum ContractEvent {
//     RequestTransactionSignature {
//         xchain_id: String,
//         sender_address: Option<XChainAddress>,
//         unsigned_transaction: String,
//         request_tokens_for_gas: Option<XChainTokenAmount>,
//     },
//     FinalizeTransactionSignature {
//         xchain_id: String,
//         sender_address: Option<XChainAddress>,
//         signed_transaction: String,
//         signed_paymaster_transaction: String,
//         request_tokens_for_gas: Option<XChainTokenAmount>,
//     },
// }

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(crate = "near_sdk::serde")]
pub struct TransactionDetails {
    signed_transaction: String,
    signed_paymaster_transaction: String,
}

#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Clone, Debug, Default)]
#[serde(crate = "near_sdk::serde")]
pub struct Flags {
    is_sender_whitelist_enabled: bool,
    is_receiver_whitelist_enabled: bool,
}

#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct GasTokenPrice {
    pub local_per_xchain: (u128, u128),
    pub updated_at_block_height: u64,
}

#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct TransactionInitiation {
    id: U64,
    pending_signature_count: u32,
}

#[derive(BorshSerialize, BorshDeserialize, BorshStorageKey, Hash, Clone, Debug, PartialEq, Eq)]
pub enum StorageKey {
    SenderWhitelist,
    ReceiverWhitelist,
    SupportedForeignChainIds,
    PendingTransactions,
}

#[derive(BorshSerialize, BorshDeserialize, PanicOnDefault, Debug, Owner)]
#[near_bindgen]
pub struct Contract {
    pub next_unique_id: u64,
    pub signer_contract_id: AccountId,
    pub oracle_id: AccountId,
    pub oracle_local_asset_id: String,
    pub oracle_xchain_asset_id: String,
    pub supported_foreign_chain_ids: UnorderedSet<u64>,
    pub sender_whitelist: UnorderedSet<XChainAddress>,
    pub receiver_whitelist: UnorderedSet<XChainAddress>,
    pub flags: Flags,
    pub price_scale: (u128, u128),
    pub pending_transactions: UnorderedMap<u64, Vec<SignatureRequest>>,
}

fn transaction_fee(
    conversion_rate: (u128, u128),
    price_scale: (u128, u128),
    request_tokens_for_gas: XChainTokenAmount,
) -> u128 {
    // calculate fee based on currently known price, and include scaling factor
    // TODO: Check price data freshness
    let a = request_tokens_for_gas * U256::from(conversion_rate.0) * U256::from(price_scale.0);
    let (b, rem) = a.div_mod(U256::from(conversion_rate.1) * U256::from(price_scale.1));
    // round up
    if rem.is_zero() { b } else { b + 1 }.as_u128()
}

#[near_bindgen]
impl Contract {
    #[init]
    pub fn new(
        signer_contract_id: AccountId,
        oracle_id: AccountId,
        oracle_local_asset_id: String,
        oracle_xchain_asset_id: String,
    ) -> Self {
        let mut contract = Self {
            next_unique_id: 0,
            signer_contract_id,
            oracle_id,
            oracle_local_asset_id,
            oracle_xchain_asset_id,
            supported_foreign_chain_ids: UnorderedSet::new(StorageKey::SupportedForeignChainIds), // TODO: Implement
            sender_whitelist: UnorderedSet::new(StorageKey::SenderWhitelist),
            receiver_whitelist: UnorderedSet::new(StorageKey::ReceiverWhitelist),
            flags: Flags::default(),
            price_scale: (120, 100), // +20% on top of market price
            pending_transactions: UnorderedMap::new(StorageKey::PendingTransactions),
        };

        Owner::init(&mut contract, &env::predecessor_account_id());

        contract
    }

    fn generate_unique_id(&mut self) -> u64 {
        let id = self.next_unique_id;
        self.next_unique_id = self.next_unique_id.checked_add(1).unwrap_or_else(|| {
            env::panic_str("Failed to generate unique ID");
        });
        id
    }

    // Public contract config getters/setters

    pub fn get_flags(&self) -> &Flags {
        &self.flags
    }

    pub fn set_flags(&mut self, flags: Flags) {
        self.assert_owner();
        self.flags = flags;
    }

    pub fn get_receiver_whitelist(&self) -> Vec<&XChainAddress> {
        self.receiver_whitelist.iter().collect()
    }

    pub fn add_to_receiver_whitelist(&mut self, addresses: Vec<XChainAddress>) {
        self.assert_owner();
        for address in addresses {
            self.receiver_whitelist.insert(address);
        }
    }

    pub fn remove_from_receiver_whitelist(&mut self, addresses: Vec<XChainAddress>) {
        self.assert_owner();
        for address in addresses {
            self.receiver_whitelist.remove(&address);
        }
    }

    pub fn clear_receiver_whitelist(&mut self) {
        self.assert_owner();
        self.receiver_whitelist.clear();
    }

    pub fn get_sender_whitelist(&self) -> Vec<&XChainAddress> {
        self.sender_whitelist.iter().collect()
    }

    pub fn add_to_sender_whitelist(&mut self, addresses: Vec<XChainAddress>) {
        self.assert_owner();
        for address in addresses {
            self.sender_whitelist.insert(address);
        }
    }

    pub fn remove_from_sender_whitelist(&mut self, addresses: Vec<XChainAddress>) {
        self.assert_owner();
        for address in addresses {
            self.sender_whitelist.remove(&address);
        }
    }

    pub fn clear_sender_whitelist(&mut self) {
        self.assert_owner();
        self.sender_whitelist.clear();
    }

    fn fetch_oracle(&mut self) -> Promise {
        ext_oracle::ext(self.oracle_id.clone()).get_price_data(Some(vec![
            self.oracle_local_asset_id.clone(),
            self.oracle_xchain_asset_id.clone(),
        ]))
    }

    fn process_oracle_result(&self, result: Result<PriceData, PromiseError>) -> GasTokenPrice {
        let price_data = result.unwrap_or_else(|_| env::panic_str("Failed to fetch price data"));

        let (local_price, xchain_price) = match &price_data.prices[..] {
            [AssetOptionalPrice {
                asset_id: first_asset_id,
                price: Some(first_price),
            }, AssetOptionalPrice {
                asset_id: second_asset_id,
                price: Some(second_price),
            }] if first_asset_id == &self.oracle_local_asset_id
                && second_asset_id == &self.oracle_xchain_asset_id =>
            {
                (first_price, second_price)
            }
            _ => env::panic_str("Invalid price data"),
        };

        GasTokenPrice {
            local_per_xchain: (
                xchain_price.multiplier.0 * u128::from(local_price.decimals),
                local_price.multiplier.0 * u128::from(xchain_price.decimals),
            ),
            updated_at_block_height: env::block_height(),
        }
    }

    // Private helper methods

    fn validate_transaction(&self, transaction: &TypedTransaction) {
        require!(
            transaction.gas().is_some() && transaction.gas_price().is_some(),
            "Gas must be explicitly specified",
        );

        require!(
            transaction.chain_id().is_some(),
            "Chain ID must be explicitly specified",
        );

        // Validate receiver
        let receiver: Option<XChainAddress> = match transaction.to() {
            Some(NameOrAddress::Name(_)) => {
                env::panic_str("ENS names are not supported");
            }
            Some(NameOrAddress::Address(address)) => Some(address.into()),
            None => None,
        };

        // Validate receiver
        if let Some(ref receiver) = receiver {
            // Check receiver whitelist
            if self.flags.is_receiver_whitelist_enabled {
                require!(
                    self.receiver_whitelist.contains(receiver),
                    "Receiver is not whitelisted",
                );
            }
        } else {
            // No receiver means contract deployment
            env::panic_str("Deployment is not allowed");
        };

        // Check sender whitelist
        if self.flags.is_sender_whitelist_enabled {
            require!(
                self.sender_whitelist.contains(
                    &transaction
                        .from()
                        .unwrap_or_else(|| env::panic_str("Sender whitelist is enabled"))
                        .into()
                ),
                "Sender is not whitelisted",
            );
        }
    }

    // Public methods

    #[payable]
    pub fn initiate_transaction(
        &mut self,
        transaction_json: Option<TypedTransaction>,
        transaction_rlp: Option<String>,
    ) -> Promise {
        let deposit = env::attached_deposit();
        require!(deposit > 0, "Deposit is required to pay for gas");

        let transaction = extract_transaction(transaction_json, transaction_rlp);

        self.validate_transaction(&transaction);

        self.fetch_oracle().then(
            Self::ext(env::current_account_id()).initiate_transaction_callback(
                env::predecessor_account_id(),
                deposit.into(),
                transaction,
            ),
        )
    }

    #[private]
    pub fn initiate_transaction_callback(
        &mut self,
        predecessor: AccountId,
        deposit: near_sdk::json_types::U128,
        transaction: TypedTransaction,
        #[callback_result] result: Result<PriceData, PromiseError>,
    ) -> TransactionInitiation {
        let gas_token_price = self.process_oracle_result(result);
        let request_tokens_for_gas = tokens_for_gas(&transaction).unwrap(); // Validation ensures gas is set.
        let fee = transaction_fee(
            gas_token_price.local_per_xchain,
            self.price_scale,
            request_tokens_for_gas,
        );
        // TODO: Ensure that deposit is returned if any recoverable errors are encountered.
        let deposit = deposit.0;

        match deposit.checked_sub(fee) {
            None => {
                env::panic_str(&format!(
                    "Attached deposit ({deposit}) is less than fee ({fee})"
                ));
            }
            Some(0) => {} // No refund; payment is exact.
            Some(refund) => {
                // Refund excess
                Promise::new(predecessor.clone()).transfer(refund);
            }
        }

        let paymaster_transaction: TypedTransaction = TransactionRequest {
            chain_id: Some(transaction.chain_id().unwrap()),
            from: None, // TODO: PK gen
            to: Some((*transaction.from().unwrap()).into()),
            value: Some(request_tokens_for_gas),
            ..Default::default()
        }
        .into();

        let transactions = vec![
            SignatureRequest::new("$", paymaster_transaction),
            SignatureRequest::new(env::predecessor_account_id(), transaction),
        ];

        let pending_signature_count = transactions.len() as u32;

        let id = self.generate_unique_id();

        self.pending_transactions.insert(id, transactions);

        TransactionInitiation {
            id: id.into(),
            pending_signature_count,
        }
    }

    pub fn sign_next(&mut self, id: U64) -> Promise {
        let id = id.0;

        let (index, next_signature_request, key_path) = self
            .pending_transactions
            .get_mut(&id)
            .unwrap_or_else(|| {
                env::panic_str(&format!("Transaction signature request {id} not found"))
            })
            .iter_mut()
            .enumerate()
            .filter_map(|(i, r)| match r.status {
                SignatureRequestStatus::Pending {
                    ref mut in_flight,
                    ref key_path,
                } if !*in_flight => {
                    *in_flight = true;
                    Some((i as u32, &r.transaction, key_path))
                }
                _ => None,
            })
            .next()
            .unwrap_or_else(|| env::panic_str("No pending or non-in-flight signature requests"));

        ext_signer::ext(self.signer_contract_id.clone()) // TODO: Gas.
            .sign(next_signature_request.0.sighash().0, key_path)
            .then(Self::ext(env::current_account_id()).sign_next_callback(id.into(), index))
    }

    #[private]
    pub fn sign_next_callback(
        &mut self,
        id: U64,
        index: u32,
        #[callback_result] result: Result<MpcSignature, PromiseError>,
    ) -> String {
        let id = id.0;

        let request = self
            .pending_transactions
            .get_mut(&id)
            .unwrap_or_else(|| env::panic_str(&format!("Pending transaction {id} not found")))
            .get_mut(index as usize)
            .unwrap_or_else(|| {
                env::panic_str(&format!(
                    "Signature request {id}.{index} not found in transaction",
                ))
            });

        if !request.is_pending() {
            env::panic_str(&format!(
                "Signature request {id}.{index} has already been signed"
            ));
        }

        let signature = result
            .unwrap_or_else(|e| env::panic_str(&format!("Failed to produce signature: {e:?}")))
            .try_into()
            .unwrap_or_else(|e| env::panic_str(&format!("Failed to decode signature: {e:?}")));

        let transaction = &request.transaction.0;

        let rlp_signed = transaction.rlp_signed(&signature);

        request.set_signature(signature);

        hex::encode(&rlp_signed)
    }
}

fn extract_transaction(
    transaction_json: Option<TypedTransaction>,
    transaction_rlp: Option<String>,
) -> TypedTransaction {
    transaction_json
        .or_else(|| {
            transaction_rlp.map(|rlp_hex| {
                let rlp_bytes = hex::decode(rlp_hex)
                    .unwrap_or_else(|_| env::panic_str("Error decoding `transaction_rlp` as hex"));
                let rlp = Rlp::new(&rlp_bytes);
                TypedTransaction::decode(&rlp).unwrap_or_else(|_| {
                    env::panic_str("Error decoding `transaction_rlp` as transaction RLP")
                })
            })
        })
        .unwrap_or_else(|| {
            env::panic_str(
                "A transaction must be provided in `transaction_json` or `transaction_rlp`",
            )
        })
}
