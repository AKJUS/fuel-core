mod collisions;

use core::num::NonZeroUsize;
use std::{
    collections::HashMap,
    iter,
    sync::Arc,
    time::{
        Instant,
        SystemTime,
    },
};

use collisions::CollisionsExt;
use fuel_core_metrics::txpool_metrics::txpool_metrics;
use fuel_core_types::{
    fuel_tx::{
        TxId,
        field::BlobId,
    },
    services::{
        transaction_status::{
            TransactionStatus,
            statuses,
        },
        txpool::{
            ArcPoolTx,
            PoolTransaction,
        },
    },
    tai64::Tai64,
};
use num_rational::Ratio;

use crate::{
    collision_manager::{
        CollisionManager,
        Collisions,
    },
    config::Config,
    error::{
        DependencyError,
        Error,
        InputValidationError,
        InsertionErrorType,
    },
    extracted_outputs::ExtractedOutputs,
    ports::{
        TxPoolPersistentStorage,
        TxStatusManager as TxStatusManagerTrait,
    },
    selection_algorithms::{
        Constraints,
        SelectionAlgorithm,
    },
    storage::{
        CheckedTransaction,
        Storage,
        StorageData,
    },
};

use crate::{
    error::RemovedReason,
    spent_inputs::SpentInputs,
};
#[cfg(test)]
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, Default)]
pub struct TxPoolStats {
    pub tx_count: u64,
    pub total_size: u64,
    pub total_gas: u64,
}

/// The pool is the main component of the txpool service. It is responsible for storing transactions
/// and allowing the selection of transactions for inclusion in a block.
pub struct Pool<S, SI, CM, SA, TxStatusManager> {
    /// Configuration of the pool.
    pub(crate) config: Config,
    /// The storage of the pool.
    pub(crate) storage: S,
    /// The collision manager of the pool.
    pub(crate) collision_manager: CM,
    /// The selection algorithm of the pool.
    pub(crate) selection_algorithm: SA,
    /// Mapping from tx_id to storage_id.
    pub(crate) tx_id_to_storage_id: HashMap<TxId, SI>,
    /// All sent outputs when transactions are extracted. Clear when processing a block.
    pub(crate) extracted_outputs: ExtractedOutputs,
    /// The spent inputs cache.
    pub(crate) spent_inputs: SpentInputs,
    /// Current pool gas stored.
    pub(crate) current_gas: u64,
    /// Current pool size in bytes.
    pub(crate) current_bytes_size: usize,
    /// The current pool gas.
    pub(crate) pool_stats_sender: tokio::sync::watch::Sender<TxPoolStats>,
    /// New executable transactions notifier.
    pub(crate) new_executable_txs_notifier: tokio::sync::watch::Sender<()>,
    /// Transaction status manager.
    pub(crate) tx_status_manager: Arc<TxStatusManager>,
}

impl<S, SI, CM, SA, TxStatusManager> Pool<S, SI, CM, SA, TxStatusManager> {
    /// Create a new pool.
    pub fn new(
        storage: S,
        collision_manager: CM,
        selection_algorithm: SA,
        config: Config,
        pool_stats_sender: tokio::sync::watch::Sender<TxPoolStats>,
        new_executable_txs_notifier: tokio::sync::watch::Sender<()>,
        tx_status_manager: Arc<TxStatusManager>,
    ) -> Self {
        let capacity = NonZeroUsize::new(config.pool_limits.max_txs.saturating_add(1))
            .expect("Max txs is greater than 0");
        let spent_inputs = SpentInputs::new(capacity);
        Pool {
            storage,
            collision_manager,
            selection_algorithm,
            config,
            tx_id_to_storage_id: HashMap::new(),
            extracted_outputs: ExtractedOutputs::new(),
            spent_inputs,
            current_gas: 0,
            current_bytes_size: 0,
            pool_stats_sender,
            new_executable_txs_notifier,
            tx_status_manager,
        }
    }

    /// Returns the number of transactions in the pool.
    pub fn tx_count(&self) -> usize {
        self.tx_id_to_storage_id.len()
    }
}

impl<S: Storage, CM, SA, TxStatusManager>
    Pool<S, S::StorageIndex, CM, SA, TxStatusManager>
where
    S: Storage,
    CM: CollisionManager<StorageIndex = S::StorageIndex>,
    SA: SelectionAlgorithm<Storage = S, StorageIndex = S::StorageIndex>,
    TxStatusManager: TxStatusManagerTrait,
{
    /// Insert transactions into the pool.
    /// Returns a list of results for each transaction.
    pub fn insert(
        &mut self,
        tx: ArcPoolTx,
        persistent_storage: &impl TxPoolPersistentStorage,
    ) -> Result<(), InsertionErrorType> {
        let tx_id = tx.id();
        if self.spent_inputs.is_spent_tx(&tx_id)
            || persistent_storage
                .contains_tx(&tx_id)
                .map_err(|e| Error::Database(format!("{:?}", e)))?
        {
            return Err(InsertionErrorType::Error(Error::InputValidation(
                InputValidationError::DuplicateTxId(tx_id),
            )))
        }

        let insertion_result = self.insert_inner(tx, persistent_storage);
        self.register_transaction_counts();
        insertion_result
    }

    fn insert_inner(
        &mut self,
        tx: Arc<PoolTransaction>,
        persistent_storage: &impl TxPoolPersistentStorage,
    ) -> Result<(), InsertionErrorType> {
        let CanStoreTransaction {
            checked_transaction,
            transactions_to_remove,
            collisions,
            _guard,
        } = self.can_insert_transaction(tx, persistent_storage)?;

        let has_dependencies = !checked_transaction.all_dependencies().is_empty();

        let mut removed_transactions = vec![];
        for tx in transactions_to_remove {
            let removed = self.storage.remove_transaction_and_dependents_subtree(tx);
            self.update_components_and_caches_on_removal(removed.iter());
            removed_transactions.extend(removed);
        }

        for collided_tx in collisions.keys() {
            let removed = self
                .storage
                .remove_transaction_and_dependents_subtree(*collided_tx);
            self.update_components_and_caches_on_removal(removed.iter());

            removed_transactions.extend(removed);
        }

        let tx = checked_transaction.tx();
        let tx_id = tx.id();
        let gas = tx.max_gas();
        let creation_instant = SystemTime::now();
        let bytes_size = tx.metered_bytes_size();

        let storage_id = self
            .storage
            .store_transaction(checked_transaction, creation_instant);

        self.current_gas = self.current_gas.saturating_add(gas);
        self.current_bytes_size = self.current_bytes_size.saturating_add(bytes_size);
        debug_assert!(!self.tx_id_to_storage_id.contains_key(&tx_id));
        self.tx_id_to_storage_id.insert(tx_id, storage_id);

        if self.config.metrics {
            txpool_metrics().tx_size.observe(bytes_size as f64);
        };

        let tx =
            Storage::get(&self.storage, &storage_id).expect("Transaction is set above");
        self.collision_manager.on_stored_transaction(storage_id, tx);

        let duration = i64::try_from(
            creation_instant
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("Time can't be less than UNIX EPOCH")
                .as_secs(),
        )
        .expect("Duration is less than i64::MAX");
        self.tx_status_manager.status_update(
            tx_id,
            TransactionStatus::submitted(Tai64::from_unix(duration)),
        );
        // No dependencies directly in the graph and the sorted transactions
        if !has_dependencies {
            self.selection_algorithm
                .new_executable_transaction(storage_id, tx);
            self.new_executable_txs_notifier.send_replace(());
        }

        let status = statuses::SqueezedOut {
            reason: Error::Removed(RemovedReason::LessWorth(tx_id)).to_string(),
        };

        let removed_transactions = removed_transactions
            .into_iter()
            .map(|data| {
                let removed_tx_id = data.transaction.id();

                (removed_tx_id, status.clone())
            })
            .collect::<Vec<_>>();
        if !removed_transactions.is_empty() {
            self.tx_status_manager
                .squeezed_out_txs(removed_transactions);
        }

        self.update_stats();
        Ok(())
    }

    fn update_stats(&self) {
        let _ = self.pool_stats_sender.send(TxPoolStats {
            tx_count: self.tx_count() as u64,
            total_size: self.current_bytes_size as u64,
            total_gas: self.current_gas,
        });
    }

    /// Check if a transaction can be inserted into the pool.
    pub fn can_insert_transaction(
        &self,
        tx: ArcPoolTx,
        persistent_storage: &impl TxPoolPersistentStorage,
    ) -> Result<CanStoreTransaction<S>, InsertionErrorType> {
        if tx.max_gas() == 0 {
            return Err(InsertionErrorType::Error(Error::InputValidation(
                InputValidationError::MaxGasZero,
            )))
        }

        let tx_id = tx.id();
        if self.tx_id_to_storage_id.contains_key(&tx_id) {
            return Err(InsertionErrorType::Error(Error::InputValidation(
                InputValidationError::DuplicateTxId(tx_id),
            )))
        }

        self.config
            .black_list
            .check_blacklisting(&tx)
            .map_err(Error::Blacklisted)?;

        Self::check_blob_does_not_exist(&tx, persistent_storage)?;
        self.storage.validate_inputs(
            &tx,
            persistent_storage,
            &self.extracted_outputs,
            &self.spent_inputs,
            self.config.utxo_validation,
        )?;

        let collisions = self.collision_manager.find_collisions(&tx)?;
        let checked_transaction = self.storage.can_store_transaction(tx)?;

        for collision in collisions.keys() {
            if checked_transaction.all_dependencies().contains(collision) {
                return Err(InsertionErrorType::Error(Error::Dependency(
                    DependencyError::NotInsertedCollisionIsDependency,
                )));
            }
        }

        let has_dependencies = !checked_transaction.all_dependencies().is_empty();

        collisions
            .check_collision_requirements(
                checked_transaction.tx(),
                has_dependencies,
                &self.storage,
            )
            .map_err(Error::Collided)?;

        let can_fit_into_pool = self.can_fit_into_pool(&checked_transaction)?;

        let mut transactions_to_remove = vec![];
        if let SpaceCheckResult::NotEnoughSpace(left) = can_fit_into_pool {
            transactions_to_remove = self.find_free_space(left, &checked_transaction)?;
        }

        let can_store_transaction = CanStoreTransaction {
            checked_transaction,
            transactions_to_remove,
            collisions,
            _guard: &self.storage,
        };

        Ok(can_store_transaction)
    }

    fn record_transaction_time_in_txpool(tx: &StorageData) {
        if let Ok(elapsed) = tx.creation_instant.elapsed() {
            txpool_metrics()
                .transaction_time_in_txpool_secs
                .observe(elapsed.as_secs_f64());
        } else {
            tracing::warn!("Failed to calculate transaction time in txpool");
        }
    }

    fn record_select_transaction_time(start: Instant) {
        let elapsed = start.elapsed().as_micros() as f64;
        txpool_metrics()
            .select_transactions_time_microseconds
            .observe(elapsed);
    }

    fn register_transaction_counts(&self) {
        if self.config.metrics {
            let num_transactions = self.tx_count();
            let executable_txs =
                self.selection_algorithm.number_of_executable_transactions();
            txpool_metrics()
                .number_of_transactions
                .set(num_transactions as i64);
            txpool_metrics()
                .number_of_executable_transactions
                .set(executable_txs as i64);
        }
    }

    /// Extract transactions for a block.
    /// Returns a list of transactions that were selected for the block
    /// based on the constraints given in the configuration and the selection algorithm used.
    pub fn extract_transactions_for_block(
        &mut self,
        constraints: Constraints,
    ) -> Vec<ArcPoolTx> {
        let metrics = self.config.metrics;
        let maybe_start = metrics.then(std::time::Instant::now);
        let best_txs = self
            .selection_algorithm
            .gather_best_txs(constraints, &mut self.storage);

        if let Some(start) = maybe_start {
            Self::record_select_transaction_time(start)
        };

        if metrics {
            best_txs.iter().for_each(|storage_data| {
                Self::record_transaction_time_in_txpool(storage_data)
            });
        }

        let txs = best_txs
            .into_iter()
            .map(|storage_entry| {
                self.extracted_outputs
                    .new_extracted_transaction(&storage_entry.transaction);
                self.spent_inputs.maybe_spend_inputs(
                    storage_entry.transaction.id(),
                    storage_entry.transaction.inputs(),
                );
                self.update_components_and_caches_on_removal(iter::once(&storage_entry));
                storage_entry.transaction
            })
            .collect::<Vec<_>>();

        self.update_stats();

        txs
    }

    pub fn get(&self, tx_id: &TxId) -> Option<&StorageData> {
        Storage::get(&self.storage, self.tx_id_to_storage_id.get(tx_id)?)
    }

    pub fn contains(&self, tx_id: &TxId) -> bool {
        self.tx_id_to_storage_id.contains_key(tx_id)
    }

    pub fn iter_tx_ids(&self) -> impl Iterator<Item = &TxId> {
        self.tx_id_to_storage_id.keys()
    }

    /// Process committed transactions:
    /// - Remove transaction but keep its dependents and the dependents become executables.
    /// - Notify about possible new executable transactions.
    pub fn process_committed_transactions(&mut self, tx_ids: impl Iterator<Item = TxId>) {
        let mut transactions_to_promote = vec![];
        for tx_id in tx_ids {
            self.spent_inputs.spend_inputs_by_tx_id(tx_id);
            if let Some(storage_id) = self.tx_id_to_storage_id.remove(&tx_id) {
                let dependents: Vec<S::StorageIndex> =
                    self.storage.get_direct_dependents(storage_id).collect();
                let Some(transaction) = self.storage.remove_transaction(storage_id)
                else {
                    debug_assert!(false, "Storage data not found for the transaction");
                    tracing::warn!(
                        "Storage data not found for the transaction during `remove_transaction`."
                    );
                    continue
                };
                self.extracted_outputs
                    .new_extracted_transaction(&transaction.transaction);
                self.spent_inputs
                    .spend_inputs(tx_id, transaction.transaction.inputs());
                self.update_components_and_caches_on_removal(iter::once(&transaction));

                for dependent in dependents {
                    if !self.storage.has_dependencies(&dependent) {
                        transactions_to_promote.push(dependent);
                    }
                }
            }
        }

        let mut new_executable_transaction = false;
        for promote in transactions_to_promote {
            let Some(storage_data) = self.storage.get(&promote) else {
                debug_assert!(
                    false,
                    "Dependent storage data not found for the transaction"
                );
                tracing::warn!(
                    "Dependent storage data not found for \
                            the transaction during `remove_transaction`."
                );
                continue
            };

            self.selection_algorithm
                .new_executable_transaction(promote, storage_data);
            new_executable_transaction = true;
        }

        if new_executable_transaction {
            self.new_executable_txs_notifier.send_replace(());
        }

        self.update_stats();
    }

    /// Check if the pool has enough space to store a transaction.
    ///
    /// It returns `true` if:
    /// - Pool is not full
    /// - Removing colliding subtree is enough to make space
    ///
    /// It returns an error if the pool is full and transactions has dependencies.
    ///
    /// If none of the above is not true, return `false`.
    fn can_fit_into_pool(
        &self,
        checked_transaction: &S::CheckedTransaction,
    ) -> Result<SpaceCheckResult, Error> {
        let tx = checked_transaction.tx();
        let tx_gas = tx.max_gas();
        let bytes_size = tx.metered_bytes_size();
        let gas_left = self.current_gas.saturating_add(tx_gas);
        let bytes_left = self.current_bytes_size.saturating_add(bytes_size);
        let txs_left = self.tx_id_to_storage_id.len().saturating_add(1);
        if gas_left <= self.config.pool_limits.max_gas
            && bytes_left <= self.config.pool_limits.max_bytes_size
            && txs_left <= self.config.pool_limits.max_txs
        {
            return Ok(SpaceCheckResult::EnoughSpace);
        }

        let has_dependencies = !checked_transaction.all_dependencies().is_empty();

        // If the transaction has a dependency and the pool is full, we refuse it
        if has_dependencies {
            return Err(Error::NotInsertedLimitHit);
        }

        let left = NotEnoughSpace {
            gas_left,
            bytes_left,
            txs_left,
        };

        Ok(SpaceCheckResult::NotEnoughSpace(left))
    }

    /// Find free space in the pool by marking less profitable transactions for removal.
    ///
    /// Return the list of transactions that must be removed from the pool along all of
    /// their dependent subtree.
    ///
    /// Returns an error impossible to find enough space.
    fn find_free_space(
        &self,
        left: NotEnoughSpace,
        checked_transaction: &S::CheckedTransaction,
    ) -> Result<Vec<S::StorageIndex>, Error> {
        let tx = checked_transaction.tx();
        let NotEnoughSpace {
            mut gas_left,
            mut bytes_left,
            mut txs_left,
        } = left;

        // Here the transaction has no dependencies which means that it's an executable transaction
        // and we want to make space for it
        let new_tx_ratio = Ratio::new(tx.tip(), tx.max_gas());

        // We want to go over executable transactions and remove the less profitable ones.
        // It is imported to go over executable transactions, not dependent ones, because we
        // can include the same subtree several times in the calculation if we use dependent txs.
        let mut sorted_txs = self.selection_algorithm.get_less_worth_txs();

        let mut transactions_to_remove = vec![];

        while gas_left > self.config.pool_limits.max_gas
            || bytes_left > self.config.pool_limits.max_bytes_size
            || txs_left > self.config.pool_limits.max_txs
        {
            let storage_id = sorted_txs.next().ok_or(Error::NotInsertedLimitHit)?;

            if checked_transaction.all_dependencies().contains(storage_id) {
                continue
            }

            debug_assert!(!self.storage.has_dependencies(storage_id));

            let Some(storage_data) = self.storage.get(storage_id) else {
                debug_assert!(
                    false,
                    "Storage data not found for one of the less worth transactions"
                );
                tracing::warn!(
                    "Storage data not found for one of the less \
                    worth transactions during `find_free_space`."
                );
                continue
            };
            let ratio = Ratio::new(
                storage_data.dependents_cumulative_tip,
                storage_data.dependents_cumulative_gas,
            );

            if ratio > new_tx_ratio {
                return Err(Error::NotInsertedLimitHit);
            }

            // It is not a correct way of deciding how many gas and bytes we will free
            // by removing this transaction, but the fastest way.
            // It will return incorrect values if several transactions have the same
            // dependents. An example:
            //
            // E - Executable transaction
            // D - Dependent transaction
            //
            //   E   E
            //    \ /
            //     D
            //    / \
            //   D   D
            //
            // Removing both E frees (E + E + 3D), while this loop assumes 2 * (E + 3D).
            // But it is okay since the limits for the TxPool are not strict.
            let gas = storage_data.dependents_cumulative_gas;
            let bytes = storage_data.dependents_cumulative_bytes_size;
            let txs = storage_data.number_dependents_in_chain;

            gas_left = gas_left.saturating_sub(gas);
            bytes_left = bytes_left.saturating_sub(bytes);
            txs_left = txs_left.saturating_sub(txs);

            transactions_to_remove.push(*storage_id);
        }

        Ok(transactions_to_remove)
    }

    /// Remove transaction and its dependents.
    pub fn remove_transactions_and_dependents<I>(
        &mut self,
        tx_ids: I,
        tx_status: statuses::SqueezedOut,
    ) where
        I: IntoIterator<Item = TxId>,
    {
        let mut removed_transactions = vec![];
        for tx_id in tx_ids {
            if let Some(storage_id) = self.tx_id_to_storage_id.remove(&tx_id) {
                let removed = self
                    .storage
                    .remove_transaction_and_dependents_subtree(storage_id);
                self.update_components_and_caches_on_removal(removed.iter());
                removed_transactions.extend(removed.into_iter().map(|data| {
                    let tx_id = data.transaction.id();

                    (tx_id, tx_status.clone())
                }));
            }
        }
        if !removed_transactions.is_empty() {
            self.tx_status_manager
                .squeezed_out_txs(removed_transactions);
        }
        self.update_stats();
    }

    pub fn remove_skipped_transaction(&mut self, tx_id: TxId, reason: String) {
        // If pre confirmation comes from the block producer node via p2p,
        // transaction still may be inside of the TxPool.
        // In that case first remove it and all its dependants.
        if self.tx_id_to_storage_id.contains_key(&tx_id) {
            let tx_status = statuses::SqueezedOut {
                reason: Error::SkippedTransaction(format!(
                    "Transaction with id: {tx_id}, was removed because of: {reason}"
                ))
                .to_string(),
            };
            self.remove_transactions_and_dependents(iter::once(tx_id), tx_status);
        }

        self.extracted_outputs.new_skipped_transaction(&tx_id);
        self.spent_inputs.unspend_inputs(tx_id);

        let coin_dependents = self.collision_manager.get_coins_spenders(&tx_id);
        if !coin_dependents.is_empty() {
            let tx_status = statuses::SqueezedOut {
                reason: Error::SkippedTransaction(format!(
                    "Parent transaction with id: {tx_id}, was removed because of: {reason}"
                ))
                .to_string(),
            };

            for dependent in coin_dependents {
                let removed = self
                    .storage
                    .remove_transaction_and_dependents_subtree(dependent);
                self.update_components_and_caches_on_removal(removed.iter());
                // It's not needed to inform the status manager about the skipped transaction herself
                // because he give the information but we need to inform him about the dependents
                // that will be deleted
                let removed_txs: Vec<_> = removed
                    .into_iter()
                    .map(|data| {
                        let tx_id = data.transaction.id();
                        (tx_id, tx_status.clone())
                    })
                    .collect();
                if !removed_txs.is_empty() {
                    self.tx_status_manager.squeezed_out_txs(removed_txs);
                }
            }
        }

        self.update_stats();
    }

    fn check_blob_does_not_exist(
        tx: &PoolTransaction,
        persistent_storage: &impl TxPoolPersistentStorage,
    ) -> Result<(), Error> {
        if let PoolTransaction::Blob(checked_tx, _) = &tx {
            let blob_id = checked_tx.transaction().blob_id();
            if persistent_storage
                .blob_exist(blob_id)
                .map_err(|e| Error::Database(format!("{:?}", e)))?
            {
                return Err(Error::InputValidation(
                    InputValidationError::NotInsertedBlobIdAlreadyTaken(*blob_id),
                ));
            }
        }
        Ok(())
    }

    fn update_components_and_caches_on_removal<'a>(
        &mut self,
        removed_transactions: impl Iterator<Item = &'a StorageData>,
    ) {
        for storage_entry in removed_transactions {
            let tx = &storage_entry.transaction;
            self.current_gas = self.current_gas.saturating_sub(tx.max_gas());
            self.current_bytes_size = self
                .current_bytes_size
                .saturating_sub(tx.metered_bytes_size());
            self.tx_id_to_storage_id.remove(&tx.id());
            self.collision_manager.on_removed_transaction(tx);
            self.selection_algorithm
                .on_removed_transaction(storage_entry);
        }
        self.register_transaction_counts();
    }

    #[cfg(test)]
    pub fn assert_integrity(&self, mut expected_txs: HashSet<TxId>) {
        for tx in &self.tx_id_to_storage_id {
            if !expected_txs.remove(tx.0) {
                panic!(
                    "Transaction with id {:?} is not in the expected transactions",
                    tx.0
                );
            }
        }
        assert!(
            expected_txs.is_empty(),
            "Some transactions are not found in the pool"
        );
    }
}

pub struct NotEnoughSpace {
    gas_left: u64,
    bytes_left: usize,
    txs_left: usize,
}

/// The result of the `can_fit_into_pool` check.
pub enum SpaceCheckResult {
    EnoughSpace,
    NotEnoughSpace(NotEnoughSpace),
}

pub struct CanStoreTransaction<'a, S>
where
    S: Storage,
{
    /// The checked transaction by the storage.
    checked_transaction: S::CheckedTransaction,
    /// List of transactions to remove to fit the new transaction into the pool.
    transactions_to_remove: Vec<S::StorageIndex>,
    /// List of collided transactions that we need to remove to insert transaction.
    collisions: Collisions<S::StorageIndex>,
    /// Protects the pool from modifications while this type is active.
    _guard: &'a S,
}
