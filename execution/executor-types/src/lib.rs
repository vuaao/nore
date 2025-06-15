// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

#![forbid(unsafe_code)]

mod error;
mod executed_chunk;

pub use error::Error;

use anyhow::Result;
use aptos_crypto::{
    ed25519::Ed25519Signature,
    hash::{EventAccumulatorHasher, TransactionAccumulatorHasher, ACCUMULATOR_PLACEHOLDER_HASH},
    HashValue,
};
use aptos_state_view::StateViewId;
use aptos_types::{
    contract_event::ContractEvent,
    epoch_state::EpochState,
    ledger_info::LedgerInfoWithSignatures,
    nibble::nibble_path::NibblePath,
    proof::{accumulator::InMemoryAccumulator, AccumulatorExtensionProof},
    state_store::{state_key::StateKey, state_value::StateValue},
    transaction::{
        Transaction, TransactionInfo, TransactionListWithProof, TransactionOutputListWithProof,
        TransactionStatus, Version,
    },
    write_set::WriteSet,
};
use scratchpad::ProofRead;
use serde::{Deserialize, Serialize};
use std::{cmp::max, collections::HashMap, sync::Arc};
use storage_interface::DbReader;

pub use executed_chunk::ExecutedChunk;
use storage_interface::{in_memory_state::InMemoryState, verified_state_view::VerifiedStateView};

type SparseMerkleProof = aptos_types::proof::SparseMerkleProof<StateValue>;

pub trait ChunkExecutorTrait: Send + Sync {
    /// Verifies the transactions based on the provided proofs and ledger info. If the transactions
    /// are valid, executes them and returns the executed result for commit.
    fn execute_chunk(
        &self,
        txn_list_with_proof: TransactionListWithProof,
        // Target LI that has been verified independently: the proofs are relative to this version.
        verified_target_li: &LedgerInfoWithSignatures,
        epoch_change_li: Option<&LedgerInfoWithSignatures>,
    ) -> Result<()>;

    /// Similar to `execute_chunk`, but instead of executing transactions, apply the transaction
    /// outputs directly to get the executed result.
    fn apply_chunk(
        &self,
        txn_output_list_with_proof: TransactionOutputListWithProof,
        // Target LI that has been verified independently: the proofs are relative to this version.
        verified_target_li: &LedgerInfoWithSignatures,
        epoch_change_li: Option<&LedgerInfoWithSignatures>,
    ) -> anyhow::Result<()>;

    /// Commit a previously executed chunk. Returns a vector of reconfiguration
    /// events in the chunk and the transactions that were committed.
    fn commit_chunk(&self) -> Result<(Vec<ContractEvent>, Vec<Transaction>)>;

    fn execute_and_commit_chunk(
        &self,
        txn_list_with_proof: TransactionListWithProof,
        verified_target_li: &LedgerInfoWithSignatures,
        epoch_change_li: Option<&LedgerInfoWithSignatures>,
    ) -> Result<(Vec<ContractEvent>, Vec<Transaction>)>;

    fn apply_and_commit_chunk(
        &self,
        txn_output_list_with_proof: TransactionOutputListWithProof,
        verified_target_li: &LedgerInfoWithSignatures,
        epoch_change_li: Option<&LedgerInfoWithSignatures>,
    ) -> Result<(Vec<ContractEvent>, Vec<Transaction>)>;

    /// Resets the chunk executor by synchronizing state with storage.
    fn reset(&self) -> Result<()>;
}

pub trait BlockExecutorTrait: Send + Sync {
    /// Get the latest committed block id
    fn committed_block_id(&self) -> HashValue;

    /// Reset the internal state including cache with newly fetched latest committed block from storage.
    fn reset(&self) -> Result<(), Error>;

    /// Executes a block.
    fn execute_block(
        &self,
        block: (HashValue, Vec<Transaction>),
        parent_block_id: HashValue,
    ) -> Result<StateComputeResult, Error>;

    /// Saves eligible blocks to persistent storage.
    /// If we have multiple blocks and not all of them have signatures, we may send them to storage
    /// in a few batches. For example, if we have
    /// ```text
    /// A <- B <- C <- D <- E
    /// ```
    /// and only `C` and `E` have signatures, we will send `A`, `B` and `C` in the first batch,
    /// then `D` and `E` later in the another batch.
    /// Commits a block and all its ancestors in a batch manner.
    fn commit_blocks(
        &self,
        block_ids: Vec<HashValue>,
        ledger_info_with_sigs: LedgerInfoWithSignatures,
    ) -> Result<(), Error>;
}

pub trait TransactionReplayer: Send {
    fn replay(
        &self,
        transactions: Vec<Transaction>,
        transaction_infos: Vec<TransactionInfo>,
    ) -> Result<()>;

    fn commit(&self) -> Result<Arc<ExecutedChunk>>;
}

/// A structure that summarizes the result of the execution needed for consensus to agree on.
/// The execution is responsible for generating the ID of the new state, which is returned in the
/// result.
///
/// Not every transaction in the payload succeeds: the returned vector keeps the boolean status
/// of success / failure of the transactions.
/// Note that the specific details of compute_status are opaque to StateMachineReplication,
/// which is going to simply pass the results between StateComputer and TxnManager.
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct StateComputeResult {
    /// transaction accumulator root hash is identified as `state_id` in Consensus.
    root_hash: HashValue,
    /// Represents the roots of all the full subtrees from left to right in this accumulator
    /// after the execution. For details, please see [`InMemoryAccumulator`](accumulator::InMemoryAccumulator).
    frozen_subtree_roots: Vec<HashValue>,

    /// The frozen subtrees roots of the parent block,
    parent_frozen_subtree_roots: Vec<HashValue>,

    /// The number of leaves of the transaction accumulator after executing a proposed block.
    /// This state must be persisted to ensure that on restart that the version is calculated correctly.
    num_leaves: u64,

    /// The number of leaves after executing the parent block,
    parent_num_leaves: u64,

    /// If set, this is the new epoch info that should be changed to if this block is committed.
    epoch_state: Option<EpochState>,
    /// The compute status (success/failure) of the given payload. The specific details are opaque
    /// for StateMachineReplication, which is merely passing it between StateComputer and
    /// TxnManager.
    compute_status: Vec<TransactionStatus>,

    /// The transaction info hashes of all success txns.
    transaction_info_hashes: Vec<HashValue>,

    /// The signature of the VoteProposal corresponding to this block.
    signature: Option<Ed25519Signature>,

    reconfig_events: Vec<ContractEvent>,
}

impl StateComputeResult {
    pub fn new(
        root_hash: HashValue,
        frozen_subtree_roots: Vec<HashValue>,
        num_leaves: u64,
        parent_frozen_subtree_roots: Vec<HashValue>,
        parent_num_leaves: u64,
        epoch_state: Option<EpochState>,
        compute_status: Vec<TransactionStatus>,
        transaction_info_hashes: Vec<HashValue>,
        reconfig_events: Vec<ContractEvent>,
    ) -> Self {
        Self {
            root_hash,
            frozen_subtree_roots,
            num_leaves,
            parent_frozen_subtree_roots,
            parent_num_leaves,
            epoch_state,
            compute_status,
            transaction_info_hashes,
            reconfig_events,
            signature: None,
        }
    }

    /// generate a new dummy state compute result with a given root hash.
    /// this function is used in RandomComputeResultStateComputer to assert that the compute
    /// function is really called.
    pub fn new_dummy_with_root_hash(root_hash: HashValue) -> Self {
        Self {
            root_hash,
            frozen_subtree_roots: vec![],
            num_leaves: 0,
            parent_frozen_subtree_roots: vec![],
            parent_num_leaves: 0,
            epoch_state: None,
            compute_status: vec![],
            transaction_info_hashes: vec![],
            reconfig_events: vec![],
            signature: None,
        }
    }

    /// generate a new dummy state compute result with ACCUMULATOR_PLACEHOLDER_HASH as the root hash.
    /// this function is used in ordering_state_computer as a dummy state compute result,
    /// where the real compute result is generated after ordering_state_computer.commit pushes
    /// the blocks and the finality proof to the execution phase.
    pub fn new_dummy() -> Self {
        StateComputeResult::new_dummy_with_root_hash(*ACCUMULATOR_PLACEHOLDER_HASH)
    }
}

impl StateComputeResult {
    pub fn version(&self) -> Version {
        max(self.num_leaves, 1)
            .checked_sub(1)
            .expect("Integer overflow occurred")
    }

    pub fn root_hash(&self) -> HashValue {
        self.root_hash
    }

    pub fn compute_status(&self) -> &Vec<TransactionStatus> {
        &self.compute_status
    }

    pub fn epoch_state(&self) -> &Option<EpochState> {
        &self.epoch_state
    }

    pub fn extension_proof(&self) -> AccumulatorExtensionProof<TransactionAccumulatorHasher> {
        AccumulatorExtensionProof::<TransactionAccumulatorHasher>::new(
            self.parent_frozen_subtree_roots.clone(),
            self.parent_num_leaves(),
            self.transaction_info_hashes().clone(),
        )
    }

    pub fn transaction_info_hashes(&self) -> &Vec<HashValue> {
        &self.transaction_info_hashes
    }

    pub fn num_leaves(&self) -> u64 {
        self.num_leaves
    }

    pub fn frozen_subtree_roots(&self) -> &Vec<HashValue> {
        &self.frozen_subtree_roots
    }

    pub fn parent_num_leaves(&self) -> u64 {
        self.parent_num_leaves
    }

    pub fn parent_frozen_subtree_roots(&self) -> &Vec<HashValue> {
        &self.parent_frozen_subtree_roots
    }

    pub fn has_reconfiguration(&self) -> bool {
        self.epoch_state.is_some()
    }

    pub fn reconfig_events(&self) -> &[ContractEvent] {
        &self.reconfig_events
    }

    pub fn signature(&self) -> &Option<Ed25519Signature> {
        &self.signature
    }

    pub fn set_signature(&mut self, sig: Ed25519Signature) {
        self.signature = Some(sig);
    }
}

/// A wrapper of the in-memory state sparse merkle tree and the transaction accumulator that
/// represent a specific state collectively. Usually it is a state after executing a block.
#[derive(Clone, Debug)]
pub struct ExecutedTrees {
    /// The in-memory representation of state after execution.
    state: InMemoryState,

    /// The in-memory Merkle Accumulator representing a blockchain state consistent with the
    /// `state_tree`.
    transaction_accumulator: Arc<InMemoryAccumulator<TransactionAccumulatorHasher>>,
}

impl ExecutedTrees {
    pub fn state(&self) -> &InMemoryState {
        &self.state
    }

    pub fn txn_accumulator(&self) -> &Arc<InMemoryAccumulator<TransactionAccumulatorHasher>> {
        &self.transaction_accumulator
    }

    pub fn version(&self) -> Option<Version> {
        let num_elements = self.txn_accumulator().num_leaves() as u64;
        num_elements.checked_sub(1)
    }

    pub fn state_id(&self) -> HashValue {
        self.txn_accumulator().root_hash()
    }

    pub fn new(
        state: InMemoryState,
        transaction_accumulator: Arc<InMemoryAccumulator<TransactionAccumulatorHasher>>,
    ) -> Self {
        Self {
            state,
            transaction_accumulator,
        }
    }

    pub fn new_at_state_checkpoint(
        state_root_hash: HashValue,
        frozen_subtrees_in_accumulator: Vec<HashValue>,
        num_leaves_in_accumulator: u64,
    ) -> Self {
        let state = InMemoryState::new_at_checkpoint(state_root_hash, num_leaves_in_accumulator);
        let transaction_accumulator = Arc::new(
            InMemoryAccumulator::new(frozen_subtrees_in_accumulator, num_leaves_in_accumulator)
                .expect("The startup info read from storage should be valid."),
        );

        Self::new(state, transaction_accumulator)
    }

    pub fn new_empty() -> Self {
        Self::new(
            InMemoryState::new_empty(),
            Arc::new(InMemoryAccumulator::new_empty()),
        )
    }

    pub fn is_same_view(&self, rhs: &Self) -> bool {
        self.transaction_accumulator.root_hash() == rhs.transaction_accumulator.root_hash()
    }

    pub fn state_view(
        &self,
        persisted_view: &Self,
        id: StateViewId,
        reader: Arc<dyn DbReader>,
    ) -> VerifiedStateView {
        VerifiedStateView::new(
            id,
            reader.clone(),
            persisted_view.state.checkpoint_version(),
            persisted_view.state.checkpoint_root_hash(),
            self.state.current.clone(),
        )
    }
}

impl Default for ExecutedTrees {
    fn default() -> Self {
        Self::new_empty()
    }
}

pub struct ProofReader {
    proofs: HashMap<HashValue, SparseMerkleProof>,
}

impl ProofReader {
    pub fn new(proofs: HashMap<HashValue, SparseMerkleProof>) -> Self {
        ProofReader { proofs }
    }

    pub fn new_empty() -> Self {
        Self::new(HashMap::new())
    }
}

impl ProofRead<StateValue> for ProofReader {
    fn get_proof(&self, key: HashValue) -> Option<&SparseMerkleProof> {
        self.proofs.get(&key)
    }
}

/// The entire set of data associated with a transaction. In addition to the output generated by VM
/// which includes the write set and events, this also has the in-memory trees.
#[derive(Clone, Debug)]
pub struct TransactionData {
    /// Each entry in this map represents the new value of a store store object touched by this
    /// transaction.
    state_updates: HashMap<StateKey, StateValue>,

    /// Each entry in this map represents the the hash of a newly generated jellyfish node
    /// and its corresponding nibble path.
    jf_node_hashes: HashMap<NibblePath, HashValue>,

    /// The writeset generated from this transaction.
    write_set: WriteSet,

    /// The list of events emitted during this transaction.
    events: Vec<ContractEvent>,

    /// List of reconfiguration events emitted during this transaction.
    reconfig_events: Vec<ContractEvent>,

    /// The execution status set by the VM.
    status: TransactionStatus,

    /// The in-memory Merkle Accumulator that has all events emitted by this transaction.
    event_tree: Arc<InMemoryAccumulator<EventAccumulatorHasher>>,

    /// The amount of gas used.
    gas_used: u64,

    /// TransactionInfo
    txn_info: TransactionInfo,

    /// TransactionInfo.hash()
    txn_info_hash: HashValue,
}

impl TransactionData {
    pub fn new(
        state_updates: HashMap<StateKey, StateValue>,
        jf_node_hashes: HashMap<NibblePath, HashValue>,
        write_set: WriteSet,
        events: Vec<ContractEvent>,
        reconfig_events: Vec<ContractEvent>,
        status: TransactionStatus,
        event_tree: Arc<InMemoryAccumulator<EventAccumulatorHasher>>,
        gas_used: u64,
        txn_info: TransactionInfo,
        txn_info_hash: HashValue,
    ) -> Self {
        TransactionData {
            state_updates,
            jf_node_hashes,
            write_set,
            events,
            reconfig_events,
            status,
            event_tree,
            gas_used,
            txn_info,
            txn_info_hash,
        }
    }

    pub fn state_updates(&self) -> &HashMap<StateKey, StateValue> {
        &self.state_updates
    }

    pub fn jf_node_hashes(&self) -> &HashMap<NibblePath, HashValue> {
        &self.jf_node_hashes
    }

    pub fn write_set(&self) -> &WriteSet {
        &self.write_set
    }

    pub fn events(&self) -> &[ContractEvent] {
        &self.events
    }

    pub fn status(&self) -> &TransactionStatus {
        &self.status
    }

    pub fn event_root_hash(&self) -> HashValue {
        self.event_tree.root_hash()
    }

    pub fn gas_used(&self) -> u64 {
        self.gas_used
    }

    pub fn txn_info_hash(&self) -> HashValue {
        self.txn_info_hash
    }
}
