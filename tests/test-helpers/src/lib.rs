use fuel_core_client::client::{
    types::TransactionStatus,
    FuelClient,
};
use fuel_core_types::{
    fuel_asm::{
        op,
        RegId,
    },
    fuel_crypto::SecretKey,
    fuel_tx::{
        policies::Policies,
        AssetId,
        Input,
        Output,
        Transaction,
        TransactionBuilder,
        Upload,
        UploadSubsection,
    },
};
use rand::{
    rngs::StdRng,
    CryptoRng,
    Rng,
    RngCore,
};

pub mod builder;
pub mod counter_contract;
pub mod fuel_core_driver;
pub mod mint_contract;

pub fn predicate() -> Vec<u8> {
    vec![op::ret(1)].into_iter().collect::<Vec<u8>>()
}

pub fn valid_input(rng: &mut StdRng, amount: u64) -> Input {
    let owner = Input::predicate_owner(predicate());
    Input::coin_predicate(
        rng.gen(),
        owner,
        amount,
        AssetId::BASE,
        Default::default(),
        Default::default(),
        predicate(),
        vec![],
    )
}

pub fn transactions_from_subsections(
    rng: &mut StdRng,
    subsections: Vec<UploadSubsection>,
    amount: u64,
) -> Vec<Upload> {
    subsections
        .into_iter()
        .map(|subsection| {
            Transaction::upload_from_subsection(
                subsection,
                Policies::new().with_max_fee(amount),
                vec![valid_input(rng, amount)],
                vec![],
                vec![],
            )
        })
        .collect()
}

pub async fn send_graph_ql_query(url: &str, query: &str) -> String {
    let client = reqwest::Client::new();
    let mut map = std::collections::HashMap::new();
    map.insert("query", query);
    let response = client.post(url).json(&map).send().await.unwrap();

    response.text().await.unwrap()
}

pub fn make_tx(
    rng: &mut (impl CryptoRng + RngCore),
    i: u64,
    max_gas_limit: u64,
) -> Transaction {
    TransactionBuilder::script(
        op::ret(RegId::ONE).to_bytes().into_iter().collect(),
        vec![],
    )
    .script_gas_limit(max_gas_limit / 2)
    .add_unsigned_coin_input(
        SecretKey::random(rng),
        rng.gen(),
        1000 + i,
        Default::default(),
        Default::default(),
    )
    .add_output(Output::Change {
        amount: 0,
        asset_id: Default::default(),
        to: rng.gen(),
    })
    .finalize_as_transaction()
}

pub async fn produce_block_with_tx(rng: &mut StdRng, client: &FuelClient) {
    let secret = SecretKey::random(rng);
    let script_tx = TransactionBuilder::script(vec![], vec![])
        .add_unsigned_coin_input(
            secret,
            rng.gen(),
            1234,
            Default::default(),
            Default::default(),
        )
        .add_output(Output::change(
            Default::default(),
            Default::default(),
            Default::default(),
        ))
        .finalize_as_transaction();
    let status = client
        .submit_and_await_commit(&script_tx)
        .await
        .expect("Failed to send tx");
    assert!(
        matches!(status, TransactionStatus::Success { .. }),
        "{status:?}"
    );
}
