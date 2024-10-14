#![allow(clippy::too_many_lines)]

use ethers_core::{
    types::{
        transaction::{eip1559::Eip1559TransactionRequest, eip2718::TypedTransaction},
        U256,
    },
    utils::{self, hex, rlp::Rlp},
};
use gas_station::{
    chain_configuration::ViewPaymasterConfiguration, contract_event::TransactionSequenceSigned,
    Nep141ReceiverCreateTransactionArgs, TransactionSequenceCreation,
};
use lib::{
    asset::AssetId,
    foreign_address::ForeignAddress,
    kdf::get_mpc_address,
    oracle::{decode_pyth_price_id, PYTH_PRICE_ID_ETH_USD, PYTH_PRICE_ID_NEAR_USD},
    pyth,
    signer::SignResult,
};
use near_sdk::{json_types::U128, serde::Deserialize, serde_json::json};
use near_workspaces::{
    network::Sandbox,
    operations::Function,
    types::{Gas, NearToken},
    Account, Contract, Worker,
};

#[allow(dead_code)]
struct Setup {
    worker: Worker<Sandbox>,
    gas_station: Contract,
    oracle: Contract,
    signer: Contract,
    nft_key: Contract,
    local_ft: Contract,
    alice: Account,
    alice_key: String,
    paymaster_key: String,
    mark_the_market_maker: Account,
}

async fn setup() -> Setup {
    let worker = near_workspaces::sandbox().await.unwrap();

    let (gas_station, oracle, signer, nft_key, local_ft, alice, mark_the_market_maker) = tokio::join!(
        async {
            let wasm = near_workspaces::compile_project("./").await.unwrap();
            worker.dev_deploy(&wasm).await.unwrap()
        },
        async {
            let wasm = near_workspaces::compile_project("../mock/oracle")
                .await
                .unwrap();
            worker.dev_deploy(&wasm).await.unwrap()
        },
        async {
            let wasm = near_workspaces::compile_project("../mock/signer")
                .await
                .unwrap();
            worker.dev_deploy(&wasm).await.unwrap()
        },
        async {
            let wasm = near_workspaces::compile_project("../nft_key")
                .await
                .unwrap();
            worker.dev_deploy(&wasm).await.unwrap()
        },
        async {
            let wasm = near_workspaces::compile_project("../mock/local_ft")
                .await
                .unwrap();
            let c = worker.dev_deploy(&wasm).await.unwrap();
            c.call("new")
                .args_json(json!({}))
                .transact()
                .await
                .unwrap()
                .unwrap();
            c
        },
        async { worker.dev_create_account().await.unwrap() },
        async { worker.dev_create_account().await.unwrap() },
    );

    println!("{:<16} {}", "Gas Station:", gas_station.id());
    println!("{:<16} {}", "Oracle:", oracle.id());
    println!("{:<16} {}", "Signer:", signer.id());
    println!("{:<16} {}", "NFT Key:", nft_key.id());
    println!("{:<16} {}", "Local FT:", local_ft.id());
    println!("{:<16} {}", "Alice:", alice.id());
    println!("{:<16} {}", "Mark:", mark_the_market_maker.id());

    println!("Initializing the contracts...");

    println!("Initializing NFT key contract...");
    nft_key
        .call("new")
        .args_json(json!({
            "signer_contract_id": signer.id(),
        }))
        .max_gas()
        .transact()
        .await
        .unwrap()
        .unwrap();

    println!("Initializing gas station contract with Alice as owner...");
    alice
        .batch(gas_station.id())
        .call(Function::new("new").args_json(json!({
            "signer_contract_id": nft_key.id(),
            "oracle_id": oracle.id(),
        })))
        .call(Function::new("add_accepted_local_asset").args_json(json!({
            "asset_id": AssetId::Native,
            "oracle_asset_id": PYTH_PRICE_ID_NEAR_USD,
            "decimals": 24,
        })))
        .call(Function::new("add_accepted_local_asset").args_json(json!({
            "asset_id": AssetId::Nep141(local_ft.id().as_str().parse().unwrap()),
            "oracle_asset_id": PYTH_PRICE_ID_ETH_USD,
            "decimals": 18,
        })))
        .call(Function::new("add_foreign_chain").args_json(json!({
            "chain_id": "0",
            "oracle_asset_id": PYTH_PRICE_ID_ETH_USD,
            "transfer_gas": "21000",
            "fee_rate": ["120", "100"],
            "decimals": 18,
        })))
        .call(Function::new("add_market_maker").args_json(json!({
            "account_id": mark_the_market_maker.id(),
        })))
        .transact()
        .await
        .unwrap()
        .unwrap();

    println!("Performing storage deposits...");
    tokio::join!(
        async {
            alice
                .call(nft_key.id(), "storage_deposit")
                .args_json(json!({}))
                .deposit(NearToken::from_near(1))
                .transact()
                .await
                .unwrap()
                .unwrap();
        },
        async {
            alice
                .call(nft_key.id(), "storage_deposit")
                .args_json(json!({
                    "account_id": gas_station.id(),
                }))
                .deposit(NearToken::from_near(1))
                .transact()
                .await
                .unwrap()
                .unwrap();
        }
    );

    println!("Generating paymaster NFT key...");
    let paymaster_key = alice
        .call(nft_key.id(), "mint")
        .max_gas()
        .transact()
        .await
        .unwrap()
        .json::<u32>()
        .unwrap()
        .to_string();

    println!("Paymaster key: {paymaster_key}");

    println!("Approving paymaster NFT key to gas station...");
    alice
        .call(nft_key.id(), "ckt_approve_call")
        .args_json(json!({
            "account_id": gas_station.id(),
            "token_id": paymaster_key,
            "msg": near_sdk::serde_json::to_string(&gas_station::ChainKeyReceiverMsg {
                is_paymaster: true,
            }).unwrap(),
        }))
        .deposit(NearToken::from_yoctonear(1))
        .max_gas()
        .transact()
        .await
        .unwrap()
        .unwrap();

    println!("Adding paymaster...");
    alice
        .call(gas_station.id(), "add_paymaster")
        .args_json(json!({
            "chain_id": "0",
            "balance": U128(10 * 10u128.pow(18)),
            "nonce": 0,
            "token_id": paymaster_key,
        }))
        .transact()
        .await
        .unwrap()
        .unwrap();

    println!("Generating Alice's NFT key...");
    let alice_key = alice
        .call(nft_key.id(), "mint")
        .max_gas()
        .transact()
        .await
        .unwrap()
        .json::<u32>()
        .unwrap()
        .to_string();

    println!("Alice's NFT key: {alice_key}");

    println!("Approving Alice's NFT key to be used by gas station...");
    alice
        .call(nft_key.id(), "ckt_approve_call")
        .args_json(json!({
            "account_id": gas_station.id(),
            "token_id": alice_key,
        }))
        .deposit(NearToken::from_yoctonear(1))
        .max_gas()
        .transact()
        .await
        .unwrap()
        .unwrap();

    println!("Initialization complete.");

    Setup {
        worker,
        gas_station,
        oracle,
        signer,
        nft_key,
        local_ft,
        alice,
        alice_key,
        paymaster_key,
        mark_the_market_maker,
    }
}

fn construct_eth_transaction(chain_id: u64) -> Eip1559TransactionRequest {
    Eip1559TransactionRequest {
        chain_id: Some(chain_id.into()),
        from: None,
        to: Some(ForeignAddress([1; 20]).into()),
        data: None,
        gas: Some(21000.into()),
        max_fee_per_gas: Some(15_000_000_000u128.into()),
        max_priority_fee_per_gas: Some(50_000_000u128.into()),
        access_list: vec![].into(),
        value: Some(100.into()),
        nonce: Some(0.into()),
    }
}

#[tokio::test]
#[should_panic = "Smart contract panicked: Attached deposit is less than fee"]
async fn fail_price_estimation_minus_one_is_insufficient() {
    let Setup {
        gas_station,
        oracle,
        alice,
        alice_key,
        ..
    } = setup().await;

    let eth_transaction = construct_eth_transaction(0);

    let (local_asset_price, foreign_asset_price) = tokio::join!(
        async {
            oracle
                .view("get_ema_price")
                .args_json(json!({
                    "price_id": pyth::PriceIdentifier(decode_pyth_price_id(PYTH_PRICE_ID_NEAR_USD)),
                }))
                .await
                .unwrap()
                .json::<pyth::Price>()
                .unwrap()
        },
        async {
            oracle
                .view("get_ema_price")
                .args_json(json!({
                    "price_id": pyth::PriceIdentifier(decode_pyth_price_id(PYTH_PRICE_ID_ETH_USD)),
                }))
                .await
                .unwrap()
                .json::<pyth::Price>()
                .unwrap()
        },
    );

    let price_estimation = gas_station
        .view("estimate_fee")
        .args_json(json!({
            "transaction_rlp_hex": hex::encode_prefixed(&eth_transaction.rlp()),
            "local_asset_price": local_asset_price,
            "local_asset_decimals": 24,
            "foreign_asset_price": foreign_asset_price,
            "foreign_asset_decimals": 18,
        }))
        .await
        .unwrap()
        .json::<U128>()
        .unwrap()
        .0;

    alice
        .call(gas_station.id(), "create_transaction")
        .args_json(json!({
            "token_id": alice_key,
            "transaction_rlp_hex": hex::encode_prefixed(&eth_transaction.rlp()),
            "use_paymaster": true,
        }))
        .deposit(NearToken::from_yoctonear(price_estimation - 1))
        .gas(Gas::from_tgas(50))
        .transact()
        .await
        .unwrap()
        .json::<TransactionSequenceCreation>()
        .unwrap();
}

#[tokio::test]
async fn test_price_estimation() {
    let Setup {
        gas_station,
        oracle,
        alice,
        alice_key,
        ..
    } = setup().await;

    let eth_transaction = construct_eth_transaction(0);

    let (local_asset_price, foreign_asset_price) = tokio::join!(
        async {
            oracle
                .view("get_ema_price")
                .args_json(json!({
                    "price_id": pyth::PriceIdentifier(decode_pyth_price_id(PYTH_PRICE_ID_NEAR_USD)),
                }))
                .await
                .unwrap()
                .json::<pyth::Price>()
                .unwrap()
        },
        async {
            oracle
                .view("get_ema_price")
                .args_json(json!({
                    "price_id": pyth::PriceIdentifier(decode_pyth_price_id(PYTH_PRICE_ID_ETH_USD)),
                }))
                .await
                .unwrap()
                .json::<pyth::Price>()
                .unwrap()
        },
    );

    let price_estimation = gas_station
        .view("estimate_fee")
        .args_json(json!({
            "transaction_rlp_hex": hex::encode_prefixed(&eth_transaction.rlp()),
            "local_asset_price": local_asset_price,
            "local_asset_decimals": 24,
            "foreign_asset_price": foreign_asset_price,
            "foreign_asset_decimals": 18,
        }))
        .await
        .unwrap()
        .json::<U128>()
        .unwrap()
        .0;

    let overall_exponent = foreign_asset_price.expo - local_asset_price.expo + 24 - 18;
    // wei * usd_eth / (10**18) / (usd_near / (10**24))

    let expected_total_maximum_gas_spend_in_eth = (eth_transaction.gas.unwrap()
        + U256::from(21000u128))
        * eth_transaction.max_fee_per_gas.unwrap();
    #[allow(clippy::cast_sign_loss)]
    let expected_total_maximum_gas_spend_in_near = {
        let mut numerator = expected_total_maximum_gas_spend_in_eth
            * (foreign_asset_price.price.0 as u64 - foreign_asset_price.conf.0)
            * 120u64;
        let mut denominator =
            U256::from(local_asset_price.price.0 as u64 + local_asset_price.conf.0) * 100u64;

        if overall_exponent < 0 {
            denominator *= 10u64.pow(-overall_exponent as u32);
        } else {
            numerator *= 10u64.pow(overall_exponent as u32);
        }

        let (t, r) = numerator.div_mod(denominator);

        if r.is_zero() {
            t
        } else {
            t + 1
        }
    }
    .as_u128();

    assert_eq!(price_estimation, expected_total_maximum_gas_spend_in_near);

    alice
        .call(gas_station.id(), "create_transaction")
        .args_json(json!({
            "token_id": alice_key,
            "transaction_rlp_hex": hex::encode_prefixed(&eth_transaction.rlp()),
            "use_paymaster": true,
        }))
        .deposit(NearToken::from_yoctonear(price_estimation))
        .gas(Gas::from_tgas(50))
        .transact()
        .await
        .unwrap()
        .json::<TransactionSequenceCreation>()
        .unwrap();
}

#[tokio::test]
#[should_panic = "Smart contract panicked: Configuration for chain ID 99999 does not exist"]
async fn fail_unsupported_chain_id() {
    let Setup {
        gas_station,
        alice,
        alice_key,
        ..
    } = setup().await;

    let eth_transaction = construct_eth_transaction(99999);

    alice
        .call(gas_station.id(), "create_transaction")
        .args_json(json!({
            "token_id": alice_key,
            "transaction_rlp_hex": hex::encode_prefixed(&eth_transaction.rlp()),
            "use_paymaster": true,
        }))
        .deposit(NearToken::from_near(1))
        .gas(Gas::from_tgas(50))
        .transact()
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn test_workflow_happy_path() {
    let Setup {
        gas_station,
        oracle,
        local_ft,
        alice,
        alice_key,
        paymaster_key,
        mark_the_market_maker,
        ..
    } = setup().await;

    println!("Checking paymaster configuration...");
    let result = gas_station
        .view("get_paymasters")
        .args_json(json!({
            "chain_id": "0",
        }))
        .await
        .unwrap()
        .json::<Vec<ViewPaymasterConfiguration>>()
        .unwrap();

    let result = &result[0];

    assert_eq!(result.nonce, 0);
    assert_eq!(
        result.minimum_available_balance,
        near_sdk::json_types::U128(10_000_000_000_000_000_000),
    );
    assert_eq!(result.token_id, paymaster_key);
    println!("Paymaster configuration check complete.");

    let eth_transaction = construct_eth_transaction(0);

    println!("Testing accepting deposits with NEP-141 token...");

    alice
        .call(local_ft.id(), "mint")
        .args_json(json!({
            "amount": near_sdk::json_types::U128(NearToken::from_near(10).as_yoctonear()),
        }))
        .transact()
        .await
        .unwrap()
        .unwrap();

    let res = alice
        .call(local_ft.id(), "ft_transfer_call")
        .args_json(json!({
            "receiver_id": gas_station.id(),
            "amount": near_sdk::json_types::U128(NearToken::from_near(1).as_yoctonear()),
            "msg": near_sdk::serde_json::to_string(&Nep141ReceiverCreateTransactionArgs {
                token_id: alice_key.clone(),
                transaction_rlp_hex: hex::encode_prefixed(&eth_transaction.rlp()),
                use_paymaster: Some(true),
            }).unwrap(),
        }))
        .deposit(NearToken::from_yoctonear(1))
        .max_gas()
        .transact()
        .await
        .unwrap();

    let id = {
        #[derive(Deserialize)]
        #[serde(crate = "near_sdk::serde")]
        struct Event {
            data: EventData,
        }

        #[derive(Deserialize)]
        #[serde(crate = "near_sdk::serde")]
        struct EventData {
            id: near_sdk::json_types::U64,
        }

        res.logs()
            .into_iter()
            .find_map(|log| {
                log.strip_prefix("EVENT_JSON:")
                    .and_then(|s| near_sdk::serde_json::from_str(s).ok())
            })
            .map(|e: Event| e.data.id)
            .unwrap()
    };

    assert_eq!(id, 0.into(), "First transaction ID");

    println!("Done testing accepting deposits with NEP-141 token.");

    println!("Creating transaction...");

    let tx = alice
        .call(gas_station.id(), "create_transaction")
        .args_json(json!({
            "token_id": alice_key,
            "transaction_rlp_hex": hex::encode_prefixed(&eth_transaction.rlp()),
            "use_paymaster": true,
        }))
        .deposit(NearToken::from_near(1))
        .gas(Gas::from_tgas(50))
        .transact()
        .await
        .unwrap()
        .json::<TransactionSequenceCreation>()
        .unwrap();

    println!("Transaction created.");

    println!("Transaction: {tx:?}");

    assert_eq!(tx.pending_signature_count, 2, "Two signatures are pending");

    println!("Dispatching first signature...");

    let signed_tx_1 = alice
        .call(gas_station.id(), "sign_next")
        .args_json(json!({
            "id": tx.id,
        }))
        .deposit(NearToken::from_yoctonear(5))
        .gas(Gas::from_tgas(50))
        .transact()
        .await
        .unwrap()
        .json::<String>()
        .unwrap();

    println!("First signed transaction: {signed_tx_1:?}");

    println!("Dispatching second signature...");

    let signed_tx_2 = alice
        .call(gas_station.id(), "sign_next")
        .args_json(json!({
            "id": tx.id,
        }))
        .deposit(NearToken::from_yoctonear(5))
        .gas(Gas::from_tgas(50))
        .transact()
        .await
        .unwrap()
        .json::<String>()
        .unwrap();

    println!("Second signed transaction: {signed_tx_2:?}");

    let alice_foreign_address = gas_station
        .view("get_foreign_address_for")
        .args_json(json!({
            "account_id": alice.id(),
            "token_id": alice_key,
        }))
        .await
        .unwrap()
        .json::<ForeignAddress>()
        .unwrap();

    let signed_transaction_bytes = hex::decode(&signed_tx_2).unwrap();
    let signed_transaction_rlp = Rlp::new(&signed_transaction_bytes);
    let (signed_tx, _s) = TypedTransaction::decode_signed(&signed_transaction_rlp).unwrap();
    assert_eq!(alice_foreign_address, signed_tx.from().unwrap().into());

    let signed_transaction_sequences = gas_station
        .view("list_signed_transaction_sequences_after")
        .args_json(json!({
            "block_height": "0",
        }))
        .await
        .unwrap()
        .json::<Vec<TransactionSequenceSigned>>()
        .unwrap();

    assert_eq!(
        signed_transaction_sequences,
        vec![TransactionSequenceSigned {
            id: tx.id,
            foreign_chain_id: "0".to_string(),
            created_by_account_id: alice.id().as_str().parse().unwrap(),
            signed_transactions: vec![signed_tx_1, signed_tx_2],
        }]
    );

    println!("List of signed transactions:");
    println!("{signed_transaction_sequences:?}");

    println!("Testing market maker withdrawals...");

    let (local_asset_price, foreign_asset_price, fees_to_withdraw) = tokio::join!(
        async {
            oracle
                .view("get_ema_price")
                .args_json(json!({
                    "price_id": pyth::PriceIdentifier(decode_pyth_price_id(PYTH_PRICE_ID_NEAR_USD)),
                }))
                .await
                .unwrap()
                .json::<pyth::Price>()
                .unwrap()
        },
        async {
            oracle
                .view("get_ema_price")
                .args_json(json!({
                    "price_id": pyth::PriceIdentifier(decode_pyth_price_id(PYTH_PRICE_ID_ETH_USD)),
                }))
                .await
                .unwrap()
                .json::<pyth::Price>()
                .unwrap()
        },
        async {
            gas_station
                .view("get_collected_fees")
                .args_json(json!({}))
                .await
                .unwrap()
                .json::<std::collections::HashMap<AssetId, U128>>()
                .unwrap()
        },
    );

    let price_estimation = gas_station
        .view("estimate_fee")
        .args_json(json!({
            "transaction_rlp_hex": hex::encode_prefixed(&eth_transaction.rlp()),
            "local_asset_price": local_asset_price,
            "local_asset_decimals": 24,
            "foreign_asset_price": foreign_asset_price,
            "foreign_asset_decimals": 18,
        }))
        .await
        .unwrap()
        .json::<U128>()
        .unwrap()
        .0;

    assert_eq!(
        price_estimation,
        fees_to_withdraw.get(&AssetId::Native).unwrap().0,
        "Exactly one transaction worth of fees are ready to be withdrawn",
    );

    let balance_before = mark_the_market_maker.view_account().await.unwrap().balance;

    let alice_cannot_withdraw_fees = alice
        .call(gas_station.id(), "withdraw_collected_fees")
        .args_json(json!({
            "asset_id": AssetId::Native,
        }))
        .deposit(NearToken::from_yoctonear(1))
        .transact()
        .await
        .unwrap();

    assert!(
        alice_cannot_withdraw_fees.is_failure(),
        "Alice is not a market maker"
    );

    mark_the_market_maker
        .call(gas_station.id(), "withdraw_collected_fees")
        .args_json(json!({
            "asset_id": AssetId::Native,
        }))
        .deposit(NearToken::from_yoctonear(1))
        .transact()
        .await
        .unwrap()
        .unwrap();

    println!("Market maker withdrawal succeeded.");

    let balance_after = mark_the_market_maker.view_account().await.unwrap().balance;

    let delta = balance_after.checked_sub(balance_before).unwrap();
    assert!(
        delta.as_yoctonear().abs_diff(price_estimation)
            < NearToken::from_millinear(1).as_yoctonear(), // allow for variation due to gas
        "One transaction worth of fees withdrawn",
    );
}

#[tokio::test]
async fn test_nft_keys_approvals_revoked() {
    let Setup {
        gas_station,
        nft_key,
        alice,
        alice_key,
        ..
    } = setup().await;

    println!("Revoking Alice's NFT key from being used by gas station...");
    alice
        .call(nft_key.id(), "ckt_revoke_call")
        .args_json(json!({
            "account_id": gas_station.id(),
            "token_id": alice_key,
        }))
        .deposit(NearToken::from_yoctonear(1))
        .max_gas()
        .transact()
        .await
        .unwrap()
        .unwrap();

    let eth_transaction = Eip1559TransactionRequest {
        chain_id: Some(0.into()),
        from: None,
        to: Some(ForeignAddress([1; 20]).into()),
        data: None,
        gas: Some(21000.into()),
        max_fee_per_gas: Some(100.into()),
        max_priority_fee_per_gas: Some(100.into()),
        access_list: vec![].into(),
        value: Some(100.into()),
        nonce: Some(0.into()),
    };

    println!("Creating transaction...");

    let tx = alice
        .call(gas_station.id(), "create_transaction")
        .args_json(json!({
            "token_id": alice_key,
            "transaction_rlp_hex": hex::encode_prefixed(&eth_transaction.rlp()),
            "use_paymaster": true,
        }))
        .deposit(NearToken::from_near(1))
        .gas(Gas::from_tgas(50))
        .transact()
        .await
        .unwrap();

    assert!(tx.is_failure(), "Contract should not have approval anymore");
}

#[test]
#[ignore = "generate a payload signable by the contract"]
fn generate_eth_rlp_hex() {
    let eth_transaction = Eip1559TransactionRequest {
        chain_id: Some(97.into()),
        from: None,
        to: Some(ForeignAddress([0x0f; 20]).into()),
        data: None,
        gas: Some(21000.into()),
        access_list: vec![].into(),
        max_fee_per_gas: Some(1234.into()),
        max_priority_fee_per_gas: Some(1234.into()),
        value: Some(1234.into()),
        nonce: Some(8802.into()),
    };

    println!("RLP: {}", hex::encode_prefixed(eth_transaction.rlp()));
    let tx: TypedTransaction = eth_transaction.into();
    let mut sighash = tx.sighash().to_fixed_bytes();
    sighash.reverse();
    println!("Sighash: {sighash:?}");
}

#[test]
fn decode_rlp() {
    // predicted address: 0x02d6ad0e6012a06ec7eb087cfcb10b8ce993b2c2
    // paymaster tx: 0x02f86a61018204d28204d28252089402d6ad0e6012a06ec7eb087cfcb10b8ce993b2c2840316d52080c080a0cc39fb05fcb8ade476f1230f8cdcab6959f46235d12df4b6a3ebd7ab8f2cce52a002c3883903979543780e68092fd4714ac7dbad71cd0b3451660d799ba40ffc9d
    // paymaster from: 0xd4ae9bbd30c1f55997aa308dedf1f3d01189bc2e
    // paymaster to: 0x02d6ad0e6012a06ec7eb087cfcb10b8ce993b2c2
    // user tx: 0x02f86a618222bb8204d28204d2825208940f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f8204d280c001a01e9f894cdcb789c70d959c44eaa8f2430856fb641e6712638635d25ca47c3cefa0514ac820e7228b6a07d849d614be54099f6cfa890d417924c830108448f8f995
    // user from: 0x02d6ad0e6012a06ec7eb087cfcb10b8ce993b2c2
    // user to: (junk)

    let bytes = hex::decode(
        "0x02f872011a8402faf08085037e11d60082520894b9a07c631d10fdce87d37eb6f18c11cbe75f1eeb878e1bc9bf04000080c001a05861ee93132033ed723d5bceb606c68f2107fc4f5ad1c36edbbf64b026381b0aa02e4398767b401a3faec153b95e639695077248b88991b57a1954a3505d998f15",
    )
    .unwrap();

    println!("{bytes:?}");

    let rlp = Rlp::new(&bytes);

    let txrq = TypedTransaction::decode_signed(&rlp).unwrap();

    println!("{txrq:?}");
}

#[test]
#[ignore]
fn test_derive_address() {
    let mpc_public_key = "secp256k1:4HFcTSodRLVCGNVcGc4Mf2fwBBBxv9jxkGdiW2S2CA1y6UpVVRWKj6RX7d7TDt65k2Bj3w9FU4BGtt43ZvuhCnNt".parse().unwrap();
    let a = get_mpc_address(mpc_public_key, &"hatchet.testnet".parse().unwrap(), "test").unwrap();
    println!("{a}");
}

#[test]
#[ignore]
fn test_derive_new_mpc() {
    let eth_transaction = Eip1559TransactionRequest {
        chain_id: Some(0.into()),
        from: None,
        to: Some(ForeignAddress([0x0f; 20]).into()),
        data: None,
        gas: Some(21000.into()),
        access_list: vec![].into(),
        max_fee_per_gas: Some(1234.into()),
        max_priority_fee_per_gas: Some(1234.into()),
        value: Some(1234.into()),
        nonce: Some(8891.into()),
    };
    let tx: TypedTransaction = eth_transaction.into();
    let sighash = tx.sighash().to_fixed_bytes();

    let mpc_signature = SignResult {
        big_r_hex: "03DAE1E75B650ABC6AD22C899FC4245A9F58E323320B7380872C1813A7DCEB0F95".to_string(),
        s_hex: "3FD2BC8430EC146E6D1B0EC64FE80EEDC0C483B95C8247FDFC5ADFC459BB3096".to_string(),
    };

    let sig: ethers_core::types::Signature = mpc_signature.try_into().unwrap();
    let recovered_address = sig.recover(sighash).unwrap();

    let signed_rlp_bytes = tx.rlp_signed(&sig);
    let signed_rlp = Rlp::new(&signed_rlp_bytes);
    let (recovered_signed_transaction, _decoded_sig) =
        TypedTransaction::decode_signed(&signed_rlp).unwrap();
    println!("{}", utils::to_checksum(&recovered_address, None));
    println!(
        "{}",
        utils::to_checksum(recovered_signed_transaction.from().unwrap(), None)
    );
    assert_eq!(
        &recovered_address,
        recovered_signed_transaction.from().unwrap()
    );
}
