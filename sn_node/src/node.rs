// Copyright 2024 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use super::{
    error::Result, event::NodeEventsChannel, quote::quotes_verification, Marker, NodeEvent,
};
#[cfg(feature = "open-metrics")]
use crate::metrics::NodeMetricsRecorder;
use crate::RunningNode;
use bytes::Bytes;
use itertools::Itertools;
use libp2p::{identity::Keypair, Multiaddr, PeerId};
use rand::{
    rngs::{OsRng, StdRng},
    thread_rng, Rng, SeedableRng,
};
use sn_evm::{AttoTokens, RewardsAddress};
#[cfg(feature = "open-metrics")]
use sn_networking::MetricsRegistries;
use sn_networking::{Instant, Network, NetworkBuilder, NetworkEvent, NodeIssue, SwarmDriver};
use sn_protocol::{
    error::Error as ProtocolError,
    messages::{ChunkProof, CmdResponse, Nonce, Query, QueryResponse, Request, Response},
    storage::RecordType,
    NetworkAddress, PrettyPrintRecordKey, CLOSE_GROUP_SIZE,
};
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{sync::mpsc::Receiver, task::spawn};

use sn_evm::EvmNetwork;

/// Interval to trigger replication of all records to all peers.
/// This is the max time it should take. Minimum interval at any node will be half this
pub const PERIODIC_REPLICATION_INTERVAL_MAX_S: u64 = 180;

/// Interval to trigger storage challenge.
/// This is the max time it should take. Minimum interval at any node will be half this
const STORE_CHALLENGE_INTERVAL_MAX_S: u64 = 7200;

/// Interval to update the nodes uptime metric
const UPTIME_METRICS_UPDATE_INTERVAL: Duration = Duration::from_secs(10);

/// Interval to clean up unrelevant records
const UNRELEVANT_RECORDS_CLEANUP_INTERVAL: Duration = Duration::from_secs(3600);

/// Helper to build and run a Node
pub struct NodeBuilder {
    identity_keypair: Keypair,
    evm_address: RewardsAddress,
    evm_network: EvmNetwork,
    addr: SocketAddr,
    initial_peers: Vec<Multiaddr>,
    local: bool,
    root_dir: PathBuf,
    #[cfg(feature = "open-metrics")]
    /// Set to Some to enable the metrics server
    metrics_server_port: Option<u16>,
    /// Enable hole punching for nodes connecting from home networks.
    pub is_behind_home_network: bool,
    #[cfg(feature = "upnp")]
    upnp: bool,
}

impl NodeBuilder {
    /// Instantiate the builder
    #[expect(clippy::too_many_arguments)]
    pub fn new(
        identity_keypair: Keypair,
        evm_address: RewardsAddress,
        evm_network: EvmNetwork,
        addr: SocketAddr,
        initial_peers: Vec<Multiaddr>,
        local: bool,
        root_dir: PathBuf,
        #[cfg(feature = "upnp")] upnp: bool,
    ) -> Self {
        Self {
            identity_keypair,
            evm_address,
            evm_network,
            addr,
            initial_peers,
            local,
            root_dir,
            #[cfg(feature = "open-metrics")]
            metrics_server_port: None,
            is_behind_home_network: false,
            #[cfg(feature = "upnp")]
            upnp,
        }
    }

    #[cfg(feature = "open-metrics")]
    /// Set the port for the OpenMetrics server. Defaults to a random port if not set
    pub fn metrics_server_port(&mut self, port: Option<u16>) {
        self.metrics_server_port = port;
    }

    /// Asynchronously runs a new node instance, setting up the swarm driver,
    /// creating a data storage, and handling network events. Returns the
    /// created `RunningNode` which contains a `NodeEventsChannel` for listening
    /// to node-related events.
    ///
    /// # Returns
    ///
    /// A `RunningNode` instance.
    ///
    /// # Errors
    ///
    /// Returns an error if there is a problem initializing the `SwarmDriver`.
    pub fn build_and_run(self) -> Result<RunningNode> {
        let mut network_builder = NetworkBuilder::new(self.identity_keypair, self.local);

        #[cfg(feature = "open-metrics")]
        let metrics_recorder = if self.metrics_server_port.is_some() {
            // metadata registry
            let mut metrics_registries = MetricsRegistries::default();
            let metrics_recorder = NodeMetricsRecorder::new(&mut metrics_registries);

            network_builder.metrics_registries(metrics_registries);

            Some(metrics_recorder)
        } else {
            None
        };

        network_builder.listen_addr(self.addr);
        #[cfg(feature = "open-metrics")]
        network_builder.metrics_server_port(self.metrics_server_port);
        network_builder.initial_peers(self.initial_peers.clone());
        network_builder.is_behind_home_network(self.is_behind_home_network);

        #[cfg(feature = "upnp")]
        network_builder.upnp(self.upnp);

        let (network, network_event_receiver, swarm_driver) =
            network_builder.build_node(self.root_dir.clone())?;
        let node_events_channel = NodeEventsChannel::default();

        let node = NodeInner {
            network: network.clone(),
            events_channel: node_events_channel.clone(),
            initial_peers: self.initial_peers,
            reward_address: self.evm_address,
            #[cfg(feature = "open-metrics")]
            metrics_recorder,
            evm_network: self.evm_network,
        };
        let node = Node {
            inner: Arc::new(node),
        };
        let running_node = RunningNode {
            network,
            node_events_channel,
            root_dir_path: self.root_dir,
            rewards_address: self.evm_address,
        };

        // Run the node
        node.run(swarm_driver, network_event_receiver);

        Ok(running_node)
    }
}

/// `Node` represents a single node in the distributed network. It handles
/// network events, processes incoming requests, interacts with the data
/// storage, and broadcasts node-related events.
#[derive(Clone)]
pub(crate) struct Node {
    inner: Arc<NodeInner>,
}

/// The actual implementation of the Node. The other is just a wrapper around this, so that we don't expose
/// the Arc from the interface.
struct NodeInner {
    events_channel: NodeEventsChannel,
    // Peers that are dialed at startup of node.
    initial_peers: Vec<Multiaddr>,
    network: Network,
    #[cfg(feature = "open-metrics")]
    metrics_recorder: Option<NodeMetricsRecorder>,
    reward_address: RewardsAddress,
    evm_network: EvmNetwork,
}

impl Node {
    /// Returns the NodeEventsChannel
    pub(crate) fn events_channel(&self) -> &NodeEventsChannel {
        &self.inner.events_channel
    }

    /// Returns the initial peers that the node will dial at startup
    pub(crate) fn initial_peers(&self) -> &Vec<Multiaddr> {
        &self.inner.initial_peers
    }

    /// Returns the instance of Network
    pub(crate) fn network(&self) -> &Network {
        &self.inner.network
    }

    #[cfg(feature = "open-metrics")]
    /// Returns a reference to the NodeMetricsRecorder if the `open-metrics` feature flag is enabled
    /// This is used to record various metrics for the node.
    pub(crate) fn metrics_recorder(&self) -> Option<&NodeMetricsRecorder> {
        self.inner.metrics_recorder.as_ref()
    }

    /// Returns the reward address of the node
    pub(crate) fn reward_address(&self) -> &RewardsAddress {
        &self.inner.reward_address
    }

    pub(crate) fn evm_network(&self) -> &EvmNetwork {
        &self.inner.evm_network
    }

    /// Runs the provided `SwarmDriver` and spawns a task to process for `NetworkEvents`
    fn run(self, swarm_driver: SwarmDriver, mut network_event_receiver: Receiver<NetworkEvent>) {
        let mut rng = StdRng::from_entropy();

        let peers_connected = Arc::new(AtomicUsize::new(0));

        let _handle = spawn(swarm_driver.run());
        let _handle = spawn(async move {
            // use a random inactivity timeout to ensure that the nodes do not sync when messages
            // are being transmitted.
            let replication_interval: u64 = rng.gen_range(
                PERIODIC_REPLICATION_INTERVAL_MAX_S / 2..PERIODIC_REPLICATION_INTERVAL_MAX_S,
            );
            let replication_interval_time = Duration::from_secs(replication_interval);
            debug!("Replication interval set to {replication_interval_time:?}");

            let mut replication_interval = tokio::time::interval(replication_interval_time);
            let _ = replication_interval.tick().await; // first tick completes immediately

            let mut uptime_metrics_update_interval =
                tokio::time::interval(UPTIME_METRICS_UPDATE_INTERVAL);
            let _ = uptime_metrics_update_interval.tick().await; // first tick completes immediately

            let mut irrelevant_records_cleanup_interval =
                tokio::time::interval(UNRELEVANT_RECORDS_CLEANUP_INTERVAL);
            let _ = irrelevant_records_cleanup_interval.tick().await; // first tick completes immediately

            // use a random neighbour storege challenge ticker to ensure
            // neighbour do not carryout challenges at the same time
            let storage_challenge_interval: u64 =
                rng.gen_range(STORE_CHALLENGE_INTERVAL_MAX_S / 2..STORE_CHALLENGE_INTERVAL_MAX_S);
            let storage_challenge_interval_time = Duration::from_secs(storage_challenge_interval);
            debug!("Storage challenge interval set to {storage_challenge_interval_time:?}");

            let mut storage_challenge_interval =
                tokio::time::interval(storage_challenge_interval_time);
            let _ = storage_challenge_interval.tick().await; // first tick completes immediately

            loop {
                let peers_connected = &peers_connected;

                tokio::select! {
                    net_event = network_event_receiver.recv() => {
                        match net_event {
                            Some(event) => {
                                let start = Instant::now();
                                let event_string = format!("{event:?}");

                                self.handle_network_event(event, peers_connected);
                                trace!("Handled non-blocking network event in {:?}: {:?}", start.elapsed(), event_string);

                            }
                            None => {
                                error!("The `NetworkEvent` channel is closed");
                                self.events_channel().broadcast(NodeEvent::ChannelClosed);
                                break;
                            }
                        }
                    }
                    // runs every replication_interval time
                    _ = replication_interval.tick() => {
                        let start = Instant::now();
                        debug!("Periodic replication triggered");
                        let network = self.network().clone();
                        self.record_metrics(Marker::IntervalReplicationTriggered);

                        let _handle = spawn(async move {
                            Self::try_interval_replication(network);
                            trace!("Periodic replication took {:?}", start.elapsed());
                        });
                    }
                    _ = uptime_metrics_update_interval.tick() => {
                        #[cfg(feature = "open-metrics")]
                        if let Some(metrics_recorder) = self.metrics_recorder() {
                            let _ = metrics_recorder.uptime.set(metrics_recorder.started_instant.elapsed().as_secs() as i64);
                        }
                    }
                    _ = irrelevant_records_cleanup_interval.tick() => {
                        let network = self.network().clone();

                        let _handle = spawn(async move {
                            Self::trigger_irrelevant_record_cleanup(network);
                        });
                    }
                    // runs every storage_challenge_interval time
                    _ = storage_challenge_interval.tick() => {
                        let start = Instant::now();
                        debug!("Periodic storage challenge triggered");
                        let network = self.network().clone();

                        let _handle = spawn(async move {
                            Self::storage_challenge(network).await;
                            trace!("Periodic storege challenge took {:?}", start.elapsed());
                        });
                    }
                }
            }
        });
    }

    /// Calls Marker::log() to insert the marker into the log files.
    /// Also calls NodeMetrics::record() to record the metric if the `open-metrics` feature flag is enabled.
    pub(crate) fn record_metrics(&self, marker: Marker) {
        marker.log();
        #[cfg(feature = "open-metrics")]
        if let Some(metrics_recorder) = self.metrics_recorder() {
            metrics_recorder.record(marker)
        }
    }

    // **** Private helpers *****

    /// Handle a network event.
    /// Spawns a thread for any likely long running tasks
    fn handle_network_event(&self, event: NetworkEvent, peers_connected: &Arc<AtomicUsize>) {
        let start = Instant::now();
        let event_string = format!("{event:?}");
        let event_header;
        debug!("Handling NetworkEvent {event_string:?}");

        match event {
            NetworkEvent::PeerAdded(peer_id, connected_peers) => {
                event_header = "PeerAdded";
                // increment peers_connected and send ConnectedToNetwork event if have connected to K_VALUE peers
                let _ = peers_connected.fetch_add(1, Ordering::SeqCst);
                if peers_connected.load(Ordering::SeqCst) == CLOSE_GROUP_SIZE {
                    self.events_channel()
                        .broadcast(NodeEvent::ConnectedToNetwork);
                }

                self.record_metrics(Marker::PeersInRoutingTable(connected_peers));
                self.record_metrics(Marker::PeerAddedToRoutingTable(&peer_id));

                // try replication here
                let network = self.network().clone();
                self.record_metrics(Marker::IntervalReplicationTriggered);
                let _handle = spawn(async move {
                    Self::try_interval_replication(network);
                });
            }
            NetworkEvent::PeerRemoved(peer_id, connected_peers) => {
                event_header = "PeerRemoved";
                self.record_metrics(Marker::PeersInRoutingTable(connected_peers));
                self.record_metrics(Marker::PeerRemovedFromRoutingTable(&peer_id));

                let network = self.network().clone();
                self.record_metrics(Marker::IntervalReplicationTriggered);
                let _handle = spawn(async move {
                    Self::try_interval_replication(network);
                });
            }
            NetworkEvent::PeerWithUnsupportedProtocol { .. } => {
                event_header = "PeerWithUnsupportedProtocol";
            }
            NetworkEvent::NewListenAddr(_) => {
                event_header = "NewListenAddr";
                if !cfg!(feature = "local") {
                    let network = self.network().clone();
                    let peers = self.initial_peers().clone();
                    let _handle = spawn(async move {
                        for addr in peers {
                            if let Err(err) = network.dial(addr.clone()).await {
                                tracing::error!("Failed to dial {addr}: {err:?}");
                            };
                        }
                    });
                }
            }
            NetworkEvent::ResponseReceived { res } => {
                event_header = "ResponseReceived";
                debug!("NetworkEvent::ResponseReceived {res:?}");
                if let Err(err) = self.handle_response(res) {
                    error!("Error while handling NetworkEvent::ResponseReceived {err:?}");
                }
            }
            NetworkEvent::KeysToFetchForReplication(keys) => {
                event_header = "KeysToFetchForReplication";
                debug!("Going to fetch {:?} keys for replication", keys.len());
                self.record_metrics(Marker::fetching_keys_for_replication(&keys));

                if let Err(err) = self.fetch_replication_keys_without_wait(keys) {
                    error!("Error while trying to fetch replicated data {err:?}");
                }
            }
            NetworkEvent::QueryRequestReceived { query, channel } => {
                event_header = "QueryRequestReceived";
                let network = self.network().clone();
                let payment_address = *self.reward_address();

                let _handle = spawn(async move {
                    let res = Self::handle_query(&network, query, payment_address).await;
                    debug!("Sending response {res:?}");

                    network.send_response(res, channel);
                });
            }
            NetworkEvent::UnverifiedRecord(record) => {
                event_header = "UnverifiedRecord";
                // queries can be long running and require validation, so we spawn a task to handle them
                let self_clone = self.clone();
                let _handle = spawn(async move {
                    let key = PrettyPrintRecordKey::from(&record.key).into_owned();
                    match self_clone.validate_and_store_record(record).await {
                        Ok(()) => debug!("UnverifiedRecord {key} has been stored"),
                        Err(err) => {
                            self_clone.record_metrics(Marker::RecordRejected(&key, &err));
                        }
                    }
                });
            }

            NetworkEvent::TerminateNode { reason } => {
                event_header = "TerminateNode";
                error!("Received termination from swarm_driver due to {reason:?}");
                self.events_channel()
                    .broadcast(NodeEvent::TerminateNode(format!("{reason:?}")));
            }
            NetworkEvent::FailedToFetchHolders(bad_nodes) => {
                event_header = "FailedToFetchHolders";
                let network = self.network().clone();
                // Note: this log will be checked in CI, and expecting `not appear`.
                //       any change to the keyword `failed to fetch` shall incur
                //       correspondent CI script change as well.
                error!("Received notification from replication_fetcher, notifying {bad_nodes:?} failed to fetch replication copies from.");
                let _handle = spawn(async move {
                    for peer_id in bad_nodes {
                        network.record_node_issues(peer_id, NodeIssue::ReplicationFailure);
                    }
                });
            }
            NetworkEvent::QuoteVerification { quotes } => {
                event_header = "QuoteVerification";
                let network = self.network().clone();

                let _handle = spawn(async move {
                    quotes_verification(&network, quotes).await;
                });
            }
            NetworkEvent::ChunkProofVerification {
                peer_id,
                key_to_verify,
            } => {
                event_header = "ChunkProofVerification";
                let network = self.network().clone();

                debug!("Going to carry out storage existence check against peer {peer_id:?}");

                let _handle = spawn(async move {
                    if chunk_proof_verify_peer(&network, peer_id, &key_to_verify).await {
                        return;
                    }
                    info!("Peer {peer_id:?} failed storage existence challenge.");
                    // TODO: shall challenge failure immediately triggers the node to be removed?
                    //       or to lower connection score once feature introduced.
                    //       If score falls too low, sever connection.
                    network.record_node_issues(peer_id, NodeIssue::FailedChunkProofCheck);
                });
            }
        }

        trace!(
            "Network handling statistics, Event {event_header:?} handled in {:?} : {event_string:?}",
            start.elapsed()
        );
    }

    // Handle the response that was not awaited at the call site
    fn handle_response(&self, response: Response) -> Result<()> {
        match response {
            Response::Cmd(CmdResponse::Replicate(Ok(()))) => {
                // This should actually have been short-circuted when received
                warn!("Mishandled replicate response, should be handled earlier");
            }
            Response::Query(QueryResponse::GetReplicatedRecord(resp)) => {
                error!("Response to replication shall be handled by called not by common handler, {resp:?}");
            }
            other => {
                warn!("handle_response not implemented for {other:?}");
            }
        };

        Ok(())
    }

    async fn handle_query(
        network: &Network,
        query: Query,
        payment_address: RewardsAddress,
    ) -> Response {
        let resp: QueryResponse = match query {
            Query::GetStoreCost(address) => {
                debug!("Got GetStoreCost request for {address:?}");
                let record_key = address.to_record_key();
                let self_id = network.peer_id();

                let store_cost = network.get_local_storecost(record_key.clone()).await;

                match store_cost {
                    Ok((cost, quoting_metrics, bad_nodes)) => {
                        if cost == AttoTokens::zero() {
                            QueryResponse::GetStoreCost {
                                quote: Err(ProtocolError::RecordExists(
                                    PrettyPrintRecordKey::from(&record_key).into_owned(),
                                )),
                                payment_address,
                                peer_address: NetworkAddress::from_peer(self_id),
                            }
                        } else {
                            QueryResponse::GetStoreCost {
                                quote: Self::create_quote_for_storecost(
                                    network,
                                    cost,
                                    &address,
                                    &quoting_metrics,
                                    bad_nodes,
                                    &payment_address,
                                ),
                                payment_address,
                                peer_address: NetworkAddress::from_peer(self_id),
                            }
                        }
                    }
                    Err(_) => QueryResponse::GetStoreCost {
                        quote: Err(ProtocolError::GetStoreCostFailed),
                        payment_address,
                        peer_address: NetworkAddress::from_peer(self_id),
                    },
                }
            }
            Query::GetRegisterRecord { requester, key } => {
                debug!("Got GetRegisterRecord from {requester:?} regarding {key:?} ");

                let our_address = NetworkAddress::from_peer(network.peer_id());
                let mut result = Err(ProtocolError::RegisterRecordNotFound {
                    holder: Box::new(our_address.clone()),
                    key: Box::new(key.clone()),
                });
                let record_key = key.as_record_key();

                if let Some(record_key) = record_key {
                    if let Ok(Some(record)) = network.get_local_record(&record_key).await {
                        result = Ok((our_address, Bytes::from(record.value)));
                    }
                }

                QueryResponse::GetRegisterRecord(result)
            }
            Query::GetReplicatedRecord { requester, key } => {
                debug!("Got GetReplicatedRecord from {requester:?} regarding {key:?}");

                let our_address = NetworkAddress::from_peer(network.peer_id());
                let mut result = Err(ProtocolError::ReplicatedRecordNotFound {
                    holder: Box::new(our_address.clone()),
                    key: Box::new(key.clone()),
                });
                let record_key = key.as_record_key();

                if let Some(record_key) = record_key {
                    if let Ok(Some(record)) = network.get_local_record(&record_key).await {
                        result = Ok((our_address, Bytes::from(record.value)));
                    }
                }

                QueryResponse::GetReplicatedRecord(result)
            }
            Query::GetChunkExistenceProof {
                key,
                nonce,
                difficulty,
            } => {
                debug!(
                    "Got GetChunkExistenceProof targeting chunk {key:?} with {difficulty} answers."
                );

                QueryResponse::GetChunkExistenceProof(
                    Self::respond_x_closest_chunk_proof(network, key, nonce, difficulty).await,
                )
            }
            Query::CheckNodeInProblem(target_address) => {
                debug!("Got CheckNodeInProblem for peer {target_address:?}");

                let is_in_trouble =
                    if let Ok(result) = network.is_peer_shunned(target_address.clone()).await {
                        result
                    } else {
                        debug!("Could not get status of {target_address:?}.");
                        false
                    };

                QueryResponse::CheckNodeInProblem {
                    reporter_address: NetworkAddress::from_peer(network.peer_id()),
                    target_address,
                    is_in_trouble,
                }
            }
        };
        Response::Query(resp)
    }

    async fn respond_x_closest_chunk_proof(
        network: &Network,
        key: NetworkAddress,
        nonce: Nonce,
        difficulty: usize,
    ) -> Vec<(NetworkAddress, Result<ChunkProof, ProtocolError>)> {
        info!("Received StorageChallenge targeting {key:?} with difficulty level of {difficulty}.");
        let mut results = vec![];
        if difficulty == 1 {
            // Client checking existence of published chunk.
            let mut result = Err(ProtocolError::ChunkDoesNotExist(key.clone()));
            if let Ok(Some(record)) = network.get_local_record(&key.to_record_key()).await {
                let proof = ChunkProof::new(&record.value, nonce);
                debug!("Chunk proof for {key:?} is {proof:?}");
                result = Ok(proof)
            } else {
                debug!("Could not get ChunkProof for {key:?} as we don't have the record locally.");
            }

            results.push((key.clone(), result));
        } else {
            let all_local_records = network.get_all_local_record_addresses().await;

            if let Ok(all_local_records) = all_local_records {
                // Only `ChunkRecord`s can be consistantly verified
                let mut all_chunk_addrs: Vec<_> = all_local_records
                    .iter()
                    .filter_map(|(addr, record_type)| {
                        if *record_type == RecordType::Chunk {
                            Some(addr.clone())
                        } else {
                            None
                        }
                    })
                    .collect();

                // Sort by distance and only take first X closest entries
                all_chunk_addrs.sort_by_key(|addr| key.distance(addr));

                // TODO: this shall be deduced from resource usage dynamically
                let workload_factor = std::cmp::min(difficulty, CLOSE_GROUP_SIZE);

                for addr in all_chunk_addrs.iter().take(workload_factor) {
                    if let Ok(Some(record)) = network.get_local_record(&addr.to_record_key()).await
                    {
                        let proof = ChunkProof::new(&record.value, nonce);
                        debug!("Chunk proof for {key:?} is {proof:?}");
                        results.push((addr.clone(), Ok(proof)));
                    }
                }
            }
        }

        info!(
            "Respond with {} answers to the StorageChallenge targeting {key:?}.",
            results.len()
        );

        results
    }

    /// Check among all chunk type records that we have,
    /// and randomly pick one as the verification candidate.
    /// This will challenge all closest peers at once.
    async fn storage_challenge(network: Network) {
        let closest_peers: Vec<PeerId> =
            if let Ok(closest_peers) = network.get_closest_k_value_local_peers().await {
                closest_peers
                    .into_iter()
                    .take(CLOSE_GROUP_SIZE)
                    .collect_vec()
            } else {
                error!("Cannot get local neighbours");
                return;
            };
        if closest_peers.len() < CLOSE_GROUP_SIZE {
            debug!(
                "Not enough neighbours ({}/{}) to carry out storage challenge.",
                closest_peers.len(),
                CLOSE_GROUP_SIZE
            );
            return;
        }

        let verify_candidates: Vec<NetworkAddress> =
            if let Ok(all_keys) = network.get_all_local_record_addresses().await {
                all_keys
                    .iter()
                    .filter_map(|(addr, record_type)| {
                        if RecordType::Chunk == *record_type {
                            Some(addr.clone())
                        } else {
                            None
                        }
                    })
                    .collect()
            } else {
                error!("Failed to get local record addresses.");
                return;
            };
        let num_of_targets = verify_candidates.len();
        if num_of_targets < 50 {
            debug!("Not enough candidates({num_of_targets}/50) to be checked against neighbours.");
            return;
        }

        info!("Starting node StorageChallenge against neighbours!");

        // TODO: launch the challenges parrallely, so that a scoring scheme can be utilized.
        for peer_id in closest_peers {
            if peer_id == network.peer_id() {
                continue;
            }

            let index: usize = OsRng.gen_range(0..num_of_targets);
            if !chunk_proof_verify_peer(&network, peer_id, &verify_candidates[index]).await {
                info!("Peer {peer_id:?} failed storage challenge.");
                // TODO: shall the challenge failure immediately triggers the node to be removed?
                network.record_node_issues(peer_id, NodeIssue::FailedChunkProofCheck);
            }
        }

        info!("Completed node StorageChallenge against neighbours!");
    }
}

async fn chunk_proof_verify_peer(network: &Network, peer_id: PeerId, key: &NetworkAddress) -> bool {
    let nonce: Nonce = thread_rng().gen::<u64>();

    let request = Request::Query(Query::GetChunkExistenceProof {
        key: key.clone(),
        nonce,
        difficulty: CLOSE_GROUP_SIZE,
    });

    let responses = network
        .send_and_get_responses(&[peer_id], &request, true)
        .await;

    // TODO: cross check with local knowledge (i.e. the claimed closest shall match locals)
    //       this also prevent peer falsely give empty or non-existent answers.

    if let Some(Ok(Response::Query(QueryResponse::GetChunkExistenceProof(answers)))) =
        responses.get(&peer_id)
    {
        if answers.is_empty() {
            info!("Peer {peer_id:?} didn't answer the ChunkProofChallenge.");
            return false;
        }
        for (addr, proof) in answers {
            if let Ok(proof) = proof {
                if let Ok(Some(record)) = network.get_local_record(&addr.to_record_key()).await {
                    let expected_proof = ChunkProof::new(&record.value, nonce);
                    // Any wrong answer shall be considered as a failure
                    if *proof != expected_proof {
                        return false;
                    }
                } else {
                    debug!(
                        "Could not get ChunkProof for {addr:?} as we don't have the record locally."
                    );
                }
            } else {
                debug!(
                    "Could not verify answer of {addr:?} from {peer_id:?} as responded with {proof:?}"
                );
            }
        }
    } else {
        info!("Peer {peer_id:?} doesn't reply the ChunkProofChallenge, or replied with error.");
        return false;
    }

    true
}
