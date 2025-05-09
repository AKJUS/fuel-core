use fuel_core_storage::{
    MerkleRoot,
    Result as StorageResult,
    StorageAsMut,
    StorageAsRef,
    column::Column,
    kv_store::KeyValueInspect,
    tables::{
        FuelBlocks,
        SealedBlockConsensus,
        Transactions,
        merkle::{
            DenseMetadataKey,
            FuelBlockMerkleMetadata,
        },
    },
    transactional::{
        Changes,
        ConflictPolicy,
        ReadTransaction,
        StorageChanges,
        StorageTransaction,
        WriteTransaction,
    },
};
use fuel_core_types::{
    blockchain::{
        SealedBlock,
        block::Block,
        consensus::Consensus,
    },
    fuel_tx::UniqueIdentifier,
    fuel_types::{
        BlockHeight,
        ChainId,
    },
    services::executor::{
        Result as ExecutorResult,
        UncommittedValidationResult,
    },
};

#[cfg_attr(any(test, feature = "test-helpers"), mockall::automock(type Database = crate::importer::test::MockDatabase;))]
/// The executors port.
pub trait Validator: Send + Sync {
    /// Executes the block and returns the result of execution with uncommitted database
    /// transaction.
    fn validate(
        &self,
        block: &Block,
    ) -> ExecutorResult<UncommittedValidationResult<Changes>>;
}

/// The trait indicates that the type supports storage transactions.
pub trait Transactional {
    /// The type of the storage transaction;
    type Transaction<'a>: DatabaseTransaction
    where
        Self: 'a;

    /// Returns the storage transaction based on the `Changes`.
    fn storage_transaction(&self, changes: Changes) -> Self::Transaction<'_>;
}

/// The alias port used by the block importer.
pub trait ImporterDatabase: Send + Sync {
    /// Returns the latest block height.
    fn latest_block_height(&self) -> StorageResult<Option<BlockHeight>>;

    /// Returns the latest block root.
    fn latest_block_root(&self) -> StorageResult<Option<MerkleRoot>>;

    /// Commit changes
    fn commit_changes(&mut self, changes: StorageChanges) -> StorageResult<()>;
}

/// The port of the storage transaction required by the importer.
#[cfg_attr(test, mockall::automock)]
pub trait DatabaseTransaction {
    /// Returns the latest block root.
    fn latest_block_root(&self) -> StorageResult<Option<MerkleRoot>>;

    /// Inserts the `SealedBlock`.
    ///
    /// The method returns `true` if the block is a new, otherwise `false`.
    // TODO: Remove `chain_id` from the signature, but for that transactions inside
    //  the block should have `cached_id`. We need to guarantee that from the Rust-type system.
    fn store_new_block(
        &mut self,
        chain_id: &ChainId,
        block: &SealedBlock,
    ) -> StorageResult<bool>;

    /// Returns the changes of the transaction.
    fn into_changes(self) -> Changes;
}

#[cfg_attr(any(test, feature = "test-helpers"), mockall::automock)]
/// The verifier of the block.
pub trait BlockVerifier: Send + Sync {
    /// Verifies the consistency of the block fields for the block's height.
    /// It includes the verification of **all** fields, it includes the consensus rules for
    /// the corresponding height.
    ///
    /// Return an error if the verification failed, otherwise `Ok(())`.
    fn verify_block_fields(
        &self,
        consensus: &Consensus,
        block: &Block,
    ) -> anyhow::Result<()>;
}

impl<S> Transactional for S
where
    S: KeyValueInspect<Column = Column>,
{
    type Transaction<'a>
        = StorageTransaction<&'a S>
    where
        Self: 'a;

    fn storage_transaction(&self, changes: Changes) -> Self::Transaction<'_> {
        self.read_transaction()
            .with_changes(changes)
            .with_policy(ConflictPolicy::Fail)
    }
}

impl<S> DatabaseTransaction for StorageTransaction<S>
where
    S: KeyValueInspect<Column = Column>,
{
    fn latest_block_root(&self) -> StorageResult<Option<MerkleRoot>> {
        Ok(self
            .storage_as_ref::<FuelBlockMerkleMetadata>()
            .get(&DenseMetadataKey::Latest)?
            .map(|cow| *cow.root()))
    }

    fn store_new_block(
        &mut self,
        chain_id: &ChainId,
        block: &SealedBlock,
    ) -> StorageResult<bool> {
        let mut storage = self.write_transaction();
        let height = block.entity.header().height();
        let mut found = storage
            .storage_as_mut::<FuelBlocks>()
            .replace(height, &block.entity.compress(chain_id))?
            .is_some();
        found |= storage
            .storage_as_mut::<SealedBlockConsensus>()
            .replace(height, &block.consensus)?
            .is_some();

        // TODO: Use `batch_insert` from https://github.com/FuelLabs/fuel-core/pull/1576
        for tx in block.entity.transactions() {
            found |= storage
                .storage_as_mut::<Transactions>()
                .replace(&tx.id(chain_id), tx)?
                .is_some();
        }
        storage.commit()?;
        Ok(!found)
    }

    fn into_changes(self) -> Changes {
        self.into_changes()
    }
}
