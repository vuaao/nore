// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use crate::{
    transaction_executor::TransactionExecutor, transaction_generator::TransactionGenerator,
    TransactionCommitter,
};
use aptos_config::{
    config::{RocksdbConfig, StoragePrunerConfig},
    utils::get_genesis_txn,
};
use aptos_jellyfish_merkle::metrics::{
    APTOS_JELLYFISH_INTERNAL_ENCODED_BYTES, APTOS_JELLYFISH_LEAF_ENCODED_BYTES,
    APTOS_JELLYFISH_STORAGE_READS,
};
use aptos_vm::AptosVM;
use aptosdb::{metrics::ROCKSDB_PROPERTIES, schema::JELLYFISH_MERKLE_NODE_CF_NAME, AptosDB};
use executor::{
    block_executor::BlockExecutor,
    db_bootstrapper::{generate_waypoint, maybe_bootstrap},
};
use executor_types::BlockExecutorTrait;
use std::{
    fs,
    path::Path,
    sync::{mpsc, Arc},
};
use storage_interface::DbReaderWriter;

pub fn run(
    num_accounts: usize,
    init_account_balance: u64,
    block_size: usize,
    db_dir: impl AsRef<Path>,
    storage_pruner_config: StoragePrunerConfig,
) {
    println!("Initializing...");

    if db_dir.as_ref().exists() {
        fs::remove_dir_all(db_dir.as_ref().join("aptosdb")).unwrap_or(());
    }
    // create if not exists
    fs::create_dir_all(db_dir.as_ref()).unwrap();

    let (config, genesis_key) = aptos_genesis_tool::test_config();
    // Create executor.
    let (db, db_rw) = DbReaderWriter::wrap(
        AptosDB::open(
            &db_dir,
            false,                 /* readonly */
            storage_pruner_config, /* pruner */
            RocksdbConfig::default(),
        )
        .expect("DB should open."),
    );

    // Bootstrap db with genesis
    let waypoint = generate_waypoint::<AptosVM>(&db_rw, get_genesis_txn(&config).unwrap()).unwrap();
    maybe_bootstrap::<AptosVM>(&db_rw, get_genesis_txn(&config).unwrap(), waypoint).unwrap();

    let executor = Arc::new(BlockExecutor::new(db_rw));
    let executor_2 = executor.clone();
    let genesis_block_id = executor.committed_block_id();
    let (block_sender, block_receiver) = mpsc::sync_channel(3 /* bound */);
    let (commit_sender, commit_receiver) = mpsc::sync_channel(3 /* bound */);

    // Set a progressing bar
    // Spawn threads to run transaction generator, executor and committer separately.
    let gen_thread = std::thread::Builder::new()
        .name("txn_generator".to_string())
        .spawn(move || {
            let mut generator =
                TransactionGenerator::new_with_sender(genesis_key, num_accounts, block_sender);
            generator.run_mint(init_account_balance, block_size);
            generator
        })
        .expect("Failed to spawn transaction generator thread.");
    let exe_thread = std::thread::Builder::new()
        .name("txn_executor".to_string())
        .spawn(move || {
            let mut exe = TransactionExecutor::new(
                executor,
                genesis_block_id,
                0, /* start_verison */
                Some(commit_sender),
            );
            while let Ok(transactions) = block_receiver.recv() {
                exe.execute_block(transactions);
            }
        })
        .expect("Failed to spawn transaction executor thread.");
    let commit_thread = std::thread::Builder::new()
        .name("txn_committer".to_string())
        .spawn(move || {
            let mut committer = TransactionCommitter::new(executor_2, 0, commit_receiver);
            committer.run();
        })
        .expect("Failed to spawn transaction committer thread.");

    // Wait for generator to finish.
    let mut generator = gen_thread.join().unwrap();
    generator.drop_sender();
    // Wait until all transactions are committed.
    exe_thread.join().unwrap();
    commit_thread.join().unwrap();
    // Do a sanity check on the sequence number to make sure all transactions are committed.
    generator.verify_sequence_number(db.clone());

    let final_version = generator.version();
    // Write metadata
    generator.write_meta(&db_dir);

    db.update_rocksdb_properties().unwrap();
    let db_size = ROCKSDB_PROPERTIES
        .with_label_values(&[
            JELLYFISH_MERKLE_NODE_CF_NAME,
            "aptos_rocksdb_live_sst_files_size_bytes",
        ])
        .get();
    let data_size = ROCKSDB_PROPERTIES
        .with_label_values(&[
            JELLYFISH_MERKLE_NODE_CF_NAME,
            "aptos_rocksdb_total-sst-files-size",
        ])
        .get();
    let reads = APTOS_JELLYFISH_STORAGE_READS.get();
    let leaf_bytes = APTOS_JELLYFISH_LEAF_ENCODED_BYTES.get();
    let internal_bytes = APTOS_JELLYFISH_INTERNAL_ENCODED_BYTES.get();
    println!("=============FINISHED DB CREATION =============");
    println!(
        "created a AptosDB til version {} with {} accounts.",
        final_version, num_accounts,
    );
    println!("DB dir: {}", db_dir.as_ref().display());
    println!("Jellyfish Merkle physical size: {}", db_size);
    println!("Jellyfish Merkle logical size: {}", data_size);
    println!("Total reads from storage: {}", reads);
    println!(
        "Total written internal nodes value size: {} bytes",
        internal_bytes
    );
    println!("Total written leaf nodes value size: {} bytes", leaf_bytes);
}
