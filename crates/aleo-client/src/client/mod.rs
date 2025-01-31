mod prover;
mod router;
mod rpc;
mod traits;
pub use prover::*;
pub use rpc::*;
pub use traits::*;

use anyhow::Result;
use async_trait::async_trait;
use colored::Colorize;
use core::{marker::PhantomData, time::Duration};
use parking_lot::RwLock;
use rand::{rngs::OsRng, CryptoRng, Rng};
use simple_log::*;
use snarkos_account::Account;
use snarkos_node_messages::{
    Data, Message, NodeType, PuzzleResponse, UnconfirmedBlock, UnconfirmedSolution,
};
use snarkos_node_router::{Heartbeat, Inbound, Outbound, Router, Routing};
use snarkos_node_tcp::{
    protocols::{Disconnect, Handshake, Reading, Writing},
    P2P,
};
use snarkvm::prelude::{
    Address, Block, CoinbasePuzzle, ConsensusStorage, EpochChallenge, FromBytes, Network,
    PrivateKey, ProverSolution, ToBytes, ViewKey,
};
use snarkvm_utilities::serialize::*;
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc,
    },
};
use tokio::sync::Mutex;

use snarkvm_algorithms::fft::{EvaluationDomain, Evaluations, Polynomial};
use snarkvm_fields::{Field, PrimeField};
use snarkvm_utilities::{cfg_iter_mut, serialize::*};
use time::OffsetDateTime;
use tokio::task::JoinHandle;
use traits::NodeInterface;

use std::{
    fmt,
    ops::{Add, AddAssign, Deref, DerefMut, Div, Mul, MulAssign, Neg, Sub, SubAssign},
};

use self::block::block_client::BlockClient;
use snarkvm_algorithms::fft::Evaluations as EvaluationsOnDomain;

/// A prover is a full node, capable of producing proofs for consensus.
#[derive(Clone)]
pub struct Prover<N: Network, C: ConsensusStorage<N>> {
    /// The account of the node.
    account: Account<N>,
    /// The router of the node.
    router: Router<N>,
    /// The coinbase puzzle.
    coinbase_puzzle: CoinbasePuzzle<N>,
    /// The latest epoch challenge.
    latest_epoch_challenge: Arc<RwLock<Option<EpochChallenge<N>>>>,
    /// The latest block.
    latest_block: Arc<RwLock<Option<Block<N>>>>,
    /// The number of puzzle instances.
    puzzle_instances: Arc<AtomicU8>,
    /// The maximum number of puzzle instances.
    max_puzzle_instances: u8,
    client_rpc: Arc<Mutex<ClientRpc>>,
    /// The spawned handles.
    handles: Arc<RwLock<Vec<JoinHandle<()>>>>,
    /// The shutdown signal.
    shutdown: Arc<AtomicBool>,
    /// PhantomData.
    _phantom: PhantomData<C>,
}

impl<N: Network, C: ConsensusStorage<N>> Prover<N, C> {
    /// Initializes a new prover node.
    pub async fn new(
        node_ip: SocketAddr,
        account: Account<N>,
        trusted_peers: &[SocketAddr],
        dev: Option<u16>,
    ) -> Result<Self> {
        // Initialize the node router.
        let router = Router::new(
            node_ip,
            NodeType::Prover,
            account.address(),
            trusted_peers,
            Self::MAXIMUM_NUMBER_OF_PEERS as u16,
            dev.is_some(),
        )
        .await?;
        // Load the coinbase puzzle.
        let coinbase_puzzle = CoinbasePuzzle::<N>::load()?;
        // Compute the maximum number of puzzle instances.
        // cpu - 2 1<x<6
        let max_puzzle_instances = 1;

        let block_client = BlockClient::connect("http://[::1]:50051")
            .await
            .expect("client rpc error");

        //num_cpus::get().saturating_sub(2).clamp(1, 6);
        // Initialize the node.
        let node = Self {
            account,
            router,
            coinbase_puzzle,
            latest_epoch_challenge: Default::default(),
            latest_block: Default::default(),
            puzzle_instances: Default::default(),
            max_puzzle_instances: u8::try_from(max_puzzle_instances)?,
            client_rpc: Arc::new(Mutex::new(rpc::ClientRpc::new(block_client))),
            handles: Default::default(),
            shutdown: Default::default(),
            _phantom: Default::default(),
        };
        // Initialize the routing.
        node.initialize_routing().await;
        // Initialize the coinbase puzzle.
        node.initialize_coinbase_puzzle().await;
        // Initialize the signal handler.
        node.handle_signals();
        // Return the node.
        Ok(node)
    }
}

#[async_trait]
impl<N: Network, C: ConsensusStorage<N>> NodeInterface<N> for Prover<N, C> {
    /// Returns the node type.
    fn node_type(&self) -> NodeType {
        self.router.node_type()
    }

    /// Returns the account private key of the node.
    fn private_key(&self) -> &PrivateKey<N> {
        self.account.private_key()
    }

    /// Returns the account view key of the node.
    fn view_key(&self) -> &ViewKey<N> {
        self.account.view_key()
    }

    /// Returns the account address of the node.
    fn address(&self) -> Address<N> {
        self.account.address()
    }

    /// Returns `true` if the node is in development mode.
    fn is_dev(&self) -> bool {
        self.router.is_dev()
    }

    /// Shuts down the node.
    async fn shut_down(&self) {
        info!("Shutting down...");

        // Shut down the coinbase puzzle.
        trace!("Shutting down the coinbase puzzle...");
        self.shutdown.store(true, Ordering::SeqCst);

        // Abort the tasks.
        trace!("Shutting down the prover...");
        self.handles.read().iter().for_each(|handle| handle.abort());

        // Shut down the router.
        self.router.shut_down().await;

        info!("Node has shut down.");
    }
}

impl<N: Network, C: ConsensusStorage<N>> Prover<N, C> {
    /// Initialize a new instance of the coinbase puzzle.
    async fn initialize_coinbase_puzzle(&self) {
        // 1  num
        for _ in 0..self.max_puzzle_instances {
            let prover = self.clone();
            self.handles.write().push(tokio::spawn(async move {
                prover.coinbase_puzzle_loop().await;
            }));
        }
    }

    /// Executes an instance of the coinbase puzzle.
    async fn coinbase_puzzle_loop(&self) {
        loop {
            // If the node is not connected to any peers, then skip this iteration.
            if self.router.number_of_connected_peers() == 0 {
                warn!("Skipping an iteration of the coinbase puzzle (no connected peers)");
                // 25s 区块，ANCHOR_TIME
                tokio::time::sleep(Duration::from_secs(N::ANCHOR_TIME as u64)).await;
                continue;
            }

            // If the number of instances of the coinbase puzzle exceeds the maximum, then skip this iteration.
            if self.num_puzzle_instances() > self.max_puzzle_instances {
                // Sleep for a brief period of time.
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }

            // Read the latest epoch challenge.
            let latest_epoch_challenge = self.latest_epoch_challenge.read().clone();
            // info!("latest_epoch_challenge{:?}", latest_epoch_challenge);
            // Read the latest state.
            let latest_state = self.latest_block.read().as_ref().map(|block| {
                (
                    block.timestamp(),
                    block.coinbase_target(),
                    block.proof_target(),
                )
            });
            // if let Some(challenge) = latest_epoch_challenge {
            //     let epoch_challenge = challenge.to_bytes_le();
            //     info!("epoch_challenge:{:?}", epoch_challenge);
            // }

            // let a = 1;
            // let state_change = latest_epoch_challenge
            //     .as_ref()
            //     .map(|c| (c.epoch_polynomial()));
            // info!("challenge:{:?}", state_change);

            // info!("latest_epoch_challenge{:?}", latest_epoch_challenge);
            // if let Some(change) = latest_epoch_challenge {
            //    info!("change:{:?}", change);
            //}
            // info!("latest_state:{:?}", latest_state);
            // info!("coinbase_puzzle: {:?}", self.coinbase_puzzle);

            // If the latest block timestamp exceeds a multiple of the anchor time, then skip this iteration.

            if let Some((latest_timestamp, coinbase_target, proof_target)) = latest_state {
                // Compute the elapsed time since the latest block.
                let elapsed = OffsetDateTime::now_utc()
                    .unix_timestamp()
                    .saturating_sub(latest_timestamp);
                // If the elapsed time exceeds a multiple of the anchor time, then skip this iteration.
                if elapsed > N::ANCHOR_TIME as i64 * 6 {
                    warn!("Skipping an iteration of the coinbase puzzle (latest block is stale)");
                    // Send a "PuzzleRequest" to a beacon node.
                    self.send_puzzle_request();
                    // self.notity();
                    // Sleep for `N::ANCHOR_TIME` seconds.
                    tokio::time::sleep(Duration::from_secs(N::ANCHOR_TIME as u64)).await;
                    let mut rpc = self.client_rpc.lock().await;

                    if let Some(challenge) = latest_epoch_challenge {
                        let epoch_vec = challenge.to_bytes_le();
                        if let Ok(epoch) = epoch_vec {
                            rpc.request_block(
                                latest_timestamp,
                                coinbase_target,
                                proof_target,
                                epoch,
                            )
                            .await;
                        }
                    }

                    continue;
                }
            }
            // if let Some((latest_timestamp, coinbase_target, proof_target)) = latest_state {
            //     let mut rpc = self.client_rpc.lock().await;
            //     rpc.request_block(latest_timestamp, coinbase_target, proof_target)
            //         .await;
            // }

            // If the latest epoch challenge and latest state exists, then proceed to generate a prover solution.
            // if let (Some(challenge), Some((_, coinbase_target, proof_target))) =
            //     (latest_epoch_challenge, latest_state)
            // {
            //     // Execute the coinbase puzzle.
            //     let prover = self.clone();
            //     let result = tokio::task::spawn_blocking(move || {
            //         prover.coinbase_puzzle_iteration(
            //             challenge,
            //             coinbase_target,
            //             proof_target,
            //             &mut OsRng,
            //         )
            //     })
            //     .await;

            //     // If the prover found a solution, then broadcast it.
            //     if let Ok(Some((solution_target, solution))) = result {
            //         info!(
            //             "Found a Solution '{}' (Proof Target {solution_target})",
            //             solution.commitment()
            //         );
            //         // Broadcast the prover solution.
            //         self.broadcast_prover_solution(solution);
            //     }
            // } else {
            //     // Otherwise, sleep for a brief period of time, to await for puzzle state.
            //     tokio::time::sleep(Duration::from_secs(1)).await;
            // }

            // If the Ctrl-C handler registered the signal, stop the prover.
            if self.shutdown.load(Ordering::Relaxed) {
                trace!("Shutting down the coinbase puzzle...");
                break;
            }
        }
    }

    /// Performs one iteration of the coinbase puzzle.
    fn coinbase_puzzle_iteration<R: Rng + CryptoRng>(
        &self,
        epoch_challenge: EpochChallenge<N>,
        coinbase_target: u64,
        proof_target: u64,
        rng: &mut R,
    ) -> Option<(u64, ProverSolution<N>)> {
        // Increment the puzzle instances.
        self.increment_puzzle_instances();

        trace!(
            "Proving 'CoinbasePuzzle' {}",
            format!(
                "(Epoch {}, Coinbase Target {coinbase_target}, Proof Target {proof_target})",
                epoch_challenge.epoch_number(),
            )
            .dimmed()
        );

        // Compute the prover solution.
        let result = self
            .coinbase_puzzle
            .prove(
                &epoch_challenge,
                self.address(),
                rng.gen(),
                Some(proof_target),
            )
            .ok()
            .and_then(|solution| {
                solution
                    .to_target()
                    .ok()
                    .map(|solution_target| (solution_target, solution))
            });

        // Decrement the puzzle instances.
        self.decrement_puzzle_instances();
        // Return the result.
        result
    }

    /// Broadcasts the prover solution to the network.
    fn broadcast_prover_solution(&self, prover_solution: ProverSolution<N>) {
        // Prepare the unconfirmed solution message.
        let message = Message::UnconfirmedSolution(UnconfirmedSolution {
            puzzle_commitment: prover_solution.commitment(),
            solution: Data::Object(prover_solution),
        });
        // Propagate the "UnconfirmedSolution" to the network.
        self.propagate(message, vec![]);
    }

    /// Returns the current number of puzzle instances.
    fn num_puzzle_instances(&self) -> u8 {
        self.puzzle_instances.load(Ordering::SeqCst)
    }

    /// Increments the number of puzzle instances.
    fn increment_puzzle_instances(&self) {
        self.puzzle_instances.fetch_add(1, Ordering::SeqCst);
        #[cfg(debug_assertions)]
        trace!("Number of Instances - {}", self.num_puzzle_instances());
    }

    /// Decrements the number of puzzle instances.
    fn decrement_puzzle_instances(&self) {
        self.puzzle_instances.fetch_sub(1, Ordering::SeqCst);
        #[cfg(debug_assertions)]
        trace!("Number of Instances - {}", self.num_puzzle_instances());
    }
}

pub mod block {
    tonic::include_proto!("block");
}
