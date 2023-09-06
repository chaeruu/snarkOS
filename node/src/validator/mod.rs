// Copyright (C) 2019-2023 Aleo Systems Inc.
// This file is part of the snarkOS library.

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at:
// http://www.apache.org/licenses/LICENSE-2.0

// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

mod router;

use crate::traits::NodeInterface;
use snarkos_account::Account;
use snarkos_node_consensus::Consensus;
use snarkos_node_messages::{
    BlockRequest,
    Message,
    NodeType,
    PuzzleResponse,
    UnconfirmedSolution,
    UnconfirmedTransaction,
};
use snarkos_node_narwhal::helpers::init_primary_channels;
use snarkos_node_rest::Rest;
use snarkos_node_router::{Heartbeat, Inbound, Outbound, Router, Routing};
use snarkos_node_tcp::{
    protocols::{Disconnect, Handshake, OnConnect, Reading, Writing},
    P2P,
};
use snarkvm::prelude::{
    block::{Block, Header},
    coinbase::ProverSolution,
    store::ConsensusStorage,
    Ledger,
    Network,
};

use anyhow::Result;
use core::future::Future;
use parking_lot::Mutex;
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::task::JoinHandle;

/// A validator is a full node, capable of validating blocks.
#[derive(Clone)]
pub struct Validator<N: Network, C: ConsensusStorage<N>> {
    /// The ledger of the node.
    ledger: Ledger<N, C>,
    /// The consensus module of the node.
    consensus: Consensus<N, C>,
    /// The router of the node.
    router: Router<N>,
    /// The REST server of the node.
    rest: Option<Rest<N, C, Self>>,
    /// The spawned handles.
    handles: Arc<Mutex<Vec<JoinHandle<()>>>>,
    /// The shutdown signal.
    shutdown: Arc<AtomicBool>,
}

impl<N: Network, C: ConsensusStorage<N>> Validator<N, C> {
    /// Initializes a new validator node.
    pub async fn new(
        node_ip: SocketAddr,
        rest_ip: Option<SocketAddr>,
        account: Account<N>,
        trusted_peers: &[SocketAddr],
        trusted_validators: &[SocketAddr],
        genesis: Block<N>,
        cdn: Option<String>,
        dev: Option<u16>,
    ) -> Result<Self> {
        // Initialize the signal handler.
        let signal_node = Self::handle_signals();

        // Initialize the ledger.
        let ledger = Ledger::load(genesis, dev)?;
        // Initialize the CDN.
        if let Some(base_url) = cdn {
            // Sync the ledger with the CDN.
            if let Err((_, error)) = snarkos_node_cdn::sync_ledger_with_cdn(&base_url, ledger.clone()).await {
                crate::helpers::log_clean_error(dev);
                return Err(error);
            }
        }
        // Initialize the consensus.
        let mut consensus = Consensus::new(account.clone(), ledger.clone(), None, trusted_validators, dev)?;
        // Initialize the primary channels.
        let (primary_sender, primary_receiver) = init_primary_channels::<N>();
        // Start the consensus.
        consensus.run(primary_sender, primary_receiver).await?;

        // Initialize the node router.
        let router = Router::new(
            node_ip,
            NodeType::Validator,
            account,
            trusted_peers,
            Self::MAXIMUM_NUMBER_OF_PEERS as u16,
            dev.is_some(),
        )
        .await?;

        // Initialize the node.
        let mut node = Self {
            ledger: ledger.clone(),
            consensus: consensus.clone(),
            router,
            rest: None,
            handles: Default::default(),
            shutdown: Default::default(),
        };
        // Initialize the transaction pool.
        node.initialize_transaction_pool(dev)?;

        // Initialize the REST server.
        if let Some(rest_ip) = rest_ip {
            node.rest = Some(Rest::start(rest_ip, Some(consensus), ledger, Arc::new(node.clone()))?);
        }
        // TODO (howardwu): The sync pool needs to be unified with the BFT, otherwise there is
        //  no trigger to advance the round when using the sync protocol to catch up.
        // // Initialize the sync pool.
        // node.initialize_sync()?;
        // Initialize the routing.
        node.initialize_routing().await;
        // Pass the node to the signal handler.
        let _ = signal_node.set(node.clone());
        // Return the node.
        Ok(node)
    }

    /// Returns the ledger.
    pub fn ledger(&self) -> &Ledger<N, C> {
        &self.ledger
    }

    /// Returns the REST server.
    pub fn rest(&self) -> &Option<Rest<N, C, Self>> {
        &self.rest
    }
}

impl<N: Network, C: ConsensusStorage<N>> Validator<N, C> {
    /// Initializes the sync pool.
    fn initialize_sync(&self) -> Result<()> {
        // Retrieve the canon locators.
        let canon_locators = crate::helpers::get_block_locators(&self.ledger)?;
        // Insert the canon locators into the sync pool.
        self.router.sync().insert_canon_locators(canon_locators).unwrap();

        // Start the sync loop.
        let validator = self.clone();
        self.handles.lock().push(tokio::spawn(async move {
            loop {
                // If the Ctrl-C handler registered the signal, stop the node.
                if validator.shutdown.load(Ordering::Relaxed) {
                    info!("Shutting down block production");
                    break;
                }

                // Sleep briefly to avoid triggering spam detection.
                tokio::time::sleep(Duration::from_secs(1)).await;

                // Prepare the block requests, if any.
                let block_requests = validator.router.sync().prepare_block_requests();
                trace!("Prepared {} block requests", block_requests.len());

                // Process the block requests.
                'outer: for (height, (hash, previous_hash, sync_ips)) in block_requests {
                    // Insert the block request into the sync pool.
                    let result =
                        validator.router.sync().insert_block_request(height, (hash, previous_hash, sync_ips.clone()));

                    // If the block request was inserted, send it to the peers.
                    if result.is_ok() {
                        // Construct the message.
                        let message =
                            Message::BlockRequest(BlockRequest { start_height: height, end_height: height + 1 });
                        // Send the message to the peers.
                        for sync_ip in sync_ips {
                            // If the send fails for any peer, remove the block request from the sync pool.
                            if validator.send(sync_ip, message.clone()).is_none() {
                                // Remove the entire block request.
                                validator.router.sync().remove_block_request(height);
                                // Break out of the loop.
                                break 'outer;
                            }
                        }
                        // Sleep for 10 milliseconds to avoid triggering spam detection.
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            }
        }));
        Ok(())
    }

    /// Attempts to advance with blocks from the sync pool.
    fn advance_with_sync_blocks(&self) {
        // Retrieve the latest block height.
        let mut current_height = self.ledger.latest_height();
        // Try to advance the ledger with the sync pool.
        while let Some(block) = self.router.sync().remove_block_response(current_height + 1) {
            // Ensure the block height matches.
            if block.height() != current_height + 1 {
                warn!("Block height mismatch: expected {}, found {}", current_height + 1, block.height());
                break;
            }
            // Check the next block.
            if let Err(error) = self.ledger.check_next_block(&block) {
                warn!("The next block ({}) is invalid - {error}", block.height());
                break;
            }
            // Attempt to advance to the next block.
            if let Err(error) = self.consensus.ledger().advance_to_next_block(&block) {
                warn!("{error}");
                break;
            }
            // Insert the height and hash as canon in the sync pool.
            self.router.sync().insert_canon_locator(block.height(), block.hash());
            // Increment the latest height.
            current_height += 1;
        }
    }

    // /// Initialize the transaction pool.
    // fn initialize_transaction_pool(&self, dev: Option<u16>) -> Result<()> {
    //     use snarkvm::{
    //         console::{
    //             account::ViewKey,
    //             program::{Identifier, Literal, Plaintext, ProgramID, Record, Value},
    //             types::U64,
    //         },
    //         ledger::block::transition::Output,
    //     };
    //     use std::str::FromStr;
    //
    //     // Initialize the locator.
    //     let locator = (ProgramID::from_str("credits.aleo")?, Identifier::from_str("split")?);
    //     // Initialize the record name.
    //     let record_name = Identifier::from_str("credits")?;
    //
    //     /// Searches the genesis block for the mint record.
    //     fn search_genesis_for_mint<N: Network>(
    //         block: Block<N>,
    //         view_key: &ViewKey<N>,
    //     ) -> Option<Record<N, Plaintext<N>>> {
    //         for transition in block.transitions().filter(|t| t.is_mint()) {
    //             if let Output::Record(_, _, Some(ciphertext)) = &transition.outputs()[0] {
    //                 if ciphertext.is_owner(view_key) {
    //                     match ciphertext.decrypt(view_key) {
    //                         Ok(record) => return Some(record),
    //                         Err(error) => {
    //                             error!("Failed to decrypt the mint output record - {error}");
    //                             return None;
    //                         }
    //                     }
    //                 }
    //             }
    //         }
    //         None
    //     }
    //
    //     /// Searches the block for the split record.
    //     fn search_block_for_split<N: Network>(
    //         block: Block<N>,
    //         view_key: &ViewKey<N>,
    //     ) -> Option<Record<N, Plaintext<N>>> {
    //         let mut found = None;
    //         // TODO (howardwu): Switch to the iterator when DoubleEndedIterator is supported.
    //         // block.transitions().rev().for_each(|t| {
    //         let splits = block.transitions().filter(|t| t.is_split()).collect::<Vec<_>>();
    //         splits.iter().rev().for_each(|t| {
    //             if found.is_some() {
    //                 return;
    //             }
    //             let Output::Record(_, _, Some(ciphertext)) = &t.outputs()[1] else {
    //                 error!("Failed to find the split output record");
    //                 return;
    //             };
    //             if ciphertext.is_owner(view_key) {
    //                 match ciphertext.decrypt(view_key) {
    //                     Ok(record) => found = Some(record),
    //                     Err(error) => {
    //                         error!("Failed to decrypt the split output record - {error}");
    //                     }
    //                 }
    //             }
    //         });
    //         found
    //     }
    //
    //     let self_ = self.clone();
    //     self.spawn(async move {
    //         // Retrieve the view key.
    //         let view_key = self_.view_key();
    //         // Initialize the record.
    //         let mut record = {
    //             let mut found = None;
    //             let mut height = self_.ledger.latest_height();
    //             while found.is_none() && height > 0 {
    //                 // Retrieve the block.
    //                 let Ok(block) = self_.ledger.get_block(height) else {
    //                     error!("Failed to get block at height {}", height);
    //                     break;
    //                 };
    //                 // Search for the latest split record.
    //                 if let Some(record) = search_block_for_split(block, view_key) {
    //                     found = Some(record);
    //                 }
    //                 // Decrement the height.
    //                 height = height.saturating_sub(1);
    //             }
    //             match found {
    //                 Some(record) => record,
    //                 None => {
    //                     // Retrieve the genesis block.
    //                     let Ok(block) = self_.ledger.get_block(0) else {
    //                         error!("Failed to get the genesis block");
    //                         return;
    //                     };
    //                     // Search the genesis block for the mint record.
    //                     if let Some(record) = search_genesis_for_mint(block, view_key) {
    //                         found = Some(record);
    //                     }
    //                     found.expect("Failed to find the split output record")
    //                 }
    //             }
    //         };
    //         info!("Starting transaction pool...");
    //         // Start the transaction loop.
    //         loop {
    //             tokio::time::sleep(Duration::from_secs(1)).await;
    //             // If the node is running in development mode, only generate if you are allowed.
    //             if let Some(dev) = dev {
    //                 if dev != 0 {
    //                     continue;
    //                 }
    //             }
    //
    //             // Prepare the inputs.
    //             let inputs = [Value::from(record.clone()), Value::from(Literal::U64(U64::new(1)))].into_iter();
    //             // Execute the transaction.
    //             let transaction = match self_.ledger.vm().execute(
    //                 self_.private_key(),
    //                 locator,
    //                 inputs,
    //                 None,
    //                 None,
    //                 &mut rand::thread_rng(),
    //             ) {
    //                 Ok(transaction) => transaction,
    //                 Err(error) => {
    //                     error!("Transaction pool encountered an execution error - {error}");
    //                     continue;
    //                 }
    //             };
    //             // Retrieve the transition.
    //             let Some(transition) = transaction.transitions().next() else {
    //                 error!("Transaction pool encountered a missing transition");
    //                 continue;
    //             };
    //             // Retrieve the second output.
    //             let Output::Record(_, _, Some(ciphertext)) = &transition.outputs()[1] else {
    //                 error!("Transaction pool encountered a missing output");
    //                 continue;
    //             };
    //             // Save the second output record.
    //             let Ok(next_record) = ciphertext.decrypt(view_key) else {
    //                 error!("Transaction pool encountered a decryption error");
    //                 continue;
    //             };
    //             // Broadcast the transaction.
    //             if self_
    //                 .unconfirmed_transaction(
    //                     self_.router.local_ip(),
    //                     UnconfirmedTransaction::from(transaction.clone()),
    //                     transaction.clone(),
    //                 )
    //                 .await
    //             {
    //                 info!("Transaction pool broadcasted the transaction");
    //                 let commitment = next_record.to_commitment(&locator.0, &record_name).unwrap();
    //                 while !self_.ledger.contains_commitment(&commitment).unwrap_or(false) {
    //                     tokio::time::sleep(Duration::from_secs(1)).await;
    //                 }
    //                 info!("Transaction accepted by the ledger");
    //             }
    //             // Save the record.
    //             record = next_record;
    //         }
    //     });
    //     Ok(())
    // }

    /// Initialize the transaction pool.
    fn initialize_transaction_pool(&self, dev: Option<u16>) -> Result<()> {
        use snarkvm::console::{
            program::{Identifier, Literal, ProgramID, Value},
            types::U64,
        };
        use std::str::FromStr;

        // Initialize the locator.
        let locator = (ProgramID::from_str("credits.aleo")?, Identifier::from_str("mint")?);

        let self_ = self.clone();
        self.spawn(async move {
            info!("Starting transaction pool...");
            // Start the transaction loop.
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                // If the node is running in development mode, only generate if you are allowed.
                if let Some(dev) = dev {
                    if dev != 0 {
                        continue;
                    }
                }

                // Prepare the inputs.
                let inputs = [Value::from(Literal::Address(self_.address())), Value::from(Literal::U64(U64::new(1)))];
                // Execute the transaction.
                let transaction = match self_.ledger.vm().execute(
                    self_.private_key(),
                    locator,
                    inputs.into_iter(),
                    None,
                    None,
                    &mut rand::thread_rng(),
                ) {
                    Ok(transaction) => transaction,
                    Err(error) => {
                        error!("Transaction pool encountered an execution error - {error}");
                        continue;
                    }
                };
                // Broadcast the transaction.
                if self_
                    .unconfirmed_transaction(
                        self_.router.local_ip(),
                        UnconfirmedTransaction::from(transaction.clone()),
                        transaction.clone(),
                    )
                    .await
                {
                    info!("Transaction pool broadcasted the transaction");
                }
            }
        });
        Ok(())
    }

    /// Spawns a task with the given future; it should only be used for long-running tasks.
    pub fn spawn<T: Future<Output = ()> + Send + 'static>(&self, future: T) {
        self.handles.lock().push(tokio::spawn(future));
    }
}

#[async_trait]
impl<N: Network, C: ConsensusStorage<N>> NodeInterface<N> for Validator<N, C> {
    /// Shuts down the node.
    async fn shut_down(&self) {
        info!("Shutting down...");

        // Shut down the sync pool.
        trace!("Shutting down the sync pool...");
        self.shutdown.store(true, Ordering::Relaxed);

        // Abort the tasks.
        trace!("Shutting down the validator...");
        self.handles.lock().iter().for_each(|handle| handle.abort());

        // Shut down the router.
        self.router.shut_down().await;

        // Shut down consensus.
        trace!("Shutting down consensus...");
        self.consensus.shut_down().await;

        info!("Node has shut down.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snarkvm::prelude::{
        store::{helpers::memory::ConsensusMemory, ConsensusStore},
        Testnet3,
        VM,
    };

    use anyhow::bail;
    use rand::SeedableRng;
    use rand_chacha::ChaChaRng;
    use std::str::FromStr;

    type CurrentNetwork = Testnet3;

    /// Use `RUST_MIN_STACK=67108864 cargo test --release profiler --features timer` to run this test.
    #[ignore]
    #[tokio::test]
    async fn test_profiler() -> Result<()> {
        // Specify the node attributes.
        let node = SocketAddr::from_str("0.0.0.0:4133").unwrap();
        let rest = SocketAddr::from_str("0.0.0.0:3033").unwrap();
        let dev = Some(0);

        // Initialize an (insecure) fixed RNG.
        let mut rng = ChaChaRng::seed_from_u64(1234567890u64);
        // Initialize the account.
        let account = Account::<CurrentNetwork>::new(&mut rng).unwrap();
        // Initialize a new VM.
        let vm = VM::from(ConsensusStore::<CurrentNetwork, ConsensusMemory<CurrentNetwork>>::open(None)?)?;
        // Initialize the genesis block.
        let genesis = vm.genesis_beacon(account.private_key(), &mut rng)?;

        println!("Initializing validator node...");

        let validator = Validator::<CurrentNetwork, ConsensusMemory<CurrentNetwork>>::new(
            node,
            Some(rest),
            account,
            &[],
            &[],
            genesis,
            None,
            dev,
        )
        .await
        .unwrap();

        println!("Loaded validator node with {} blocks", validator.ledger.latest_height(),);

        bail!("\n\nRemember to #[ignore] this test!\n\n")
    }
}
