use crate::{
    database::{
        OffChainIterableKeyValueView,
        OnChainIterableKeyValueView,
    },
    fuel_core_graphql_api::storage::coins::{
        OwnedCoins,
        owner_coin_id_key,
    },
};
use fuel_core_storage::{
    Result as StorageResult,
    StorageAsRef,
    iter::{
        IterDirection,
        IteratorOverTable,
    },
    not_found,
    tables::Coins,
};
use fuel_core_types::{
    entities::coins::coin::CompressedCoin,
    fuel_tx::{
        Address,
        TxId,
        UtxoId,
    },
};

impl OffChainIterableKeyValueView {
    pub fn owned_coins_ids(
        &self,
        owner: &Address,
        start_coin: Option<UtxoId>,
        direction: Option<IterDirection>,
    ) -> impl Iterator<Item = StorageResult<UtxoId>> + '_ + use<'_> {
        let start_coin = start_coin.map(|b| owner_coin_id_key(owner, &b));
        self.iter_all_filtered_keys::<OwnedCoins, _>(
            Some(*owner),
            start_coin.as_ref(),
            direction,
        )
        .map(|res| {
            res.map(|key| {
                UtxoId::new(
                    TxId::try_from(&key[32..64]).expect("The slice has size 32"),
                    u16::from_be_bytes(
                        key[64..].try_into().expect("The slice has size 2"),
                    ),
                )
            })
        })
    }
}

impl OnChainIterableKeyValueView {
    pub fn coin(&self, utxo_id: &UtxoId) -> StorageResult<CompressedCoin> {
        let coin = self
            .storage_as_ref::<Coins>()
            .get(utxo_id)?
            .ok_or(not_found!(Coins))?
            .into_owned();

        Ok(coin)
    }
}
