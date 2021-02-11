use super::{BurnchainController, BurnchainTip, Config, EventDispatcher, Keychain};
use crate::config::HELIUM_BLOCK_LIMIT;
use crate::run_loop::RegisteredKey;
use std::collections::HashMap;

use std::cmp;
use std::collections::{HashSet, VecDeque};
use std::convert::{TryFrom, TryInto};
use std::default::Default;
use std::fs;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::{thread, thread::JoinHandle};

use stacks::burnchains::{Burnchain, BurnchainHeaderHash, BurnchainParameters, Txid};
use stacks::chainstate::burn::db::sortdb::{SortitionDB, SortitionId};
use stacks::chainstate::burn::operations::{
    leader_block_commit::{RewardSetInfo, BURN_BLOCK_MINED_AT_MODULUS},
    BlockstackOperationType, LeaderBlockCommitOp, LeaderKeyRegisterOp,
};
use stacks::chainstate::burn::BlockSnapshot;
use stacks::chainstate::burn::{BlockHeaderHash, ConsensusHash, VRFSeed};
use stacks::chainstate::stacks::db::unconfirmed::UnconfirmedTxMap;
use stacks::chainstate::stacks::db::{
    ChainStateBootData, ClarityTx, StacksChainState, MINER_REWARD_MATURITY,
};
use stacks::chainstate::stacks::Error as ChainstateError;
use stacks::chainstate::stacks::StacksPublicKey;
use stacks::chainstate::stacks::{miner::StacksMicroblockBuilder, StacksBlockBuilder};
use stacks::chainstate::stacks::{
    CoinbasePayload, StacksAddress, StacksBlock, StacksBlockHeader, StacksMicroblock,
    StacksTransaction, StacksTransactionSigner, TransactionAnchorMode, TransactionPayload,
    TransactionVersion,
};
use stacks::core::mempool::MemPoolDB;
use stacks::deps::bitcoin::util::hash::Sha256dHash;
use stacks::net::{
    atlas::{AtlasConfig, AtlasDB, AttachmentInstance},
    db::{LocalPeer, PeerDB},
    dns::DNSResolver,
    p2p::PeerNetwork,
    relay::Relayer,
    rpc::{ RPCDirective, RPCHandlerArgs },
    BuildBlockTemplateResponse,
    BuildBlockTemplateRPCResponse::BuildBlockTemplateRPCResponseOk,
    BuildBlockTemplateRPCResponse::BuildBlockTemplateRPCResponseErr,
    RegisterKeyResponse,
    RegisterKeyRPCResponse::RegisterKeyRPCResponseOk,
    RegisterKeyRPCResponse::RegisterKeyRPCResponseErr,
    Error as NetError, NetworkResult, PeerAddress, StacksMessageCodec,
};
use stacks::util::get_epoch_time_ms;
use stacks::util::get_epoch_time_secs;
use stacks::util::hash::{to_hex, Hash160, Sha256Sum};
use stacks::util::secp256k1::Secp256k1PrivateKey;
use stacks::util::strings::{UrlString, VecDisplay};
use stacks::util::vrf::{ VRFPrivateKey, VRFPublicKey };
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError, TrySendError};

use crate::burnchains::bitcoin_regtest_controller::BitcoinRegtestController;
use crate::syncctl::PoxSyncWatchdogComms;

use crate::ChainTip;
use stacks::burnchains::BurnchainSigner;
use stacks::core::FIRST_BURNCHAIN_CONSENSUS_HASH;

use stacks::chainstate::coordinator::comm::CoordinatorChannels;
use stacks::chainstate::coordinator::{get_next_recipients, get_next_recipientsPSQ, OnChainRewardSetProvider};

use stacks::monitoring::{increment_stx_blocks_mined_counter, update_active_miners_count_gauge};

pub const RPC_MAX_BUFFER: usize = 100;
pub const RELAYER_MAX_BUFFER: usize = 100;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistrationKey {
    pub block_height: u64,
    pub op_vtxindex: u32,
    pub vrf_public_key: VRFPublicKey,
    pub vrf_secret_key: VRFPrivateKey,
    pub txid: Sha256dHash,
}

#[derive(Clone, Debug)]
pub struct AssembledAnchorBlock {
    parent_consensus_hash: ConsensusHash,
    my_burn_hash: BurnchainHeaderHash,
    anchored_block: StacksBlock,
    attempt: u64,
}

struct MicroblockMinerState {
    parent_consensus_hash: ConsensusHash,
    parent_block_hash: BlockHeaderHash,
    miner_key: Secp256k1PrivateKey,
    frequency: u64,
    last_mined: u128,
    quantity: u64,
}

enum RelayerDirective {
    HandleNetResult(NetworkResult),
    ProcessTenure(ConsensusHash, BurnchainHeaderHash, BlockHeaderHash),
    RunTenure(RegisteredKey, BlockSnapshot),
    RegisterKey(BlockSnapshot),
    RunMicroblockTenure,
}

pub struct InitializedNeonNode {
    config: Config,
    relay_channel: SyncSender<RelayerDirective>,
    burnchain_signer: BurnchainSigner,
    last_burn_block: Option<BlockSnapshot>,
    sleep_before_tenure: u64,
    is_miner: bool,
    pub atlas_config: AtlasConfig,
    leader_key_registration_state: LeaderKeyRegistrationState,
}

pub struct NeonGenesisNode {
    pub config: Config,
    keychain: Keychain,
    event_dispatcher: EventDispatcher,
    burnchain: Burnchain,
}

#[cfg(test)]
type BlocksProcessedCounter = std::sync::Arc<std::sync::atomic::AtomicU64>;

#[cfg(not(test))]
type BlocksProcessedCounter = ();

#[cfg(test)]
fn bump_processed_counter(blocks_processed: &BlocksProcessedCounter) {
    blocks_processed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(not(test))]
fn bump_processed_counter(_blocks_processed: &BlocksProcessedCounter) {}

/// Process artifacts from the tenure.
/// At this point, we're modifying the chainstate, and merging the artifacts from the previous tenure.
fn inner_process_tenure(
    anchored_block: &StacksBlock,
    consensus_hash: &ConsensusHash,
    parent_consensus_hash: &ConsensusHash,
    burn_db: &mut SortitionDB,
    chain_state: &mut StacksChainState,
    coord_comms: &CoordinatorChannels,
) -> Result<bool, ChainstateError> {
    let stacks_blocks_processed = coord_comms.get_stacks_blocks_processed();

    let ic = burn_db.index_conn();

    // Preprocess the anchored block
    chain_state.preprocess_anchored_block(
        &ic,
        consensus_hash,
        &anchored_block,
        &parent_consensus_hash,
        0,
    )?;

    if !coord_comms.announce_new_stacks_block() {
        return Ok(false);
    }
    if !coord_comms.wait_for_stacks_blocks_processed(stacks_blocks_processed, 15000) {
        warn!("ChainsCoordinator timed out while waiting for new stacks block to be processed");
    }

    Ok(true)
}

fn inner_generate_coinbase_tx(
    keychain: &mut Keychain,
    nonce: u64,
    is_mainnet: bool,
    chain_id: u32,
) -> StacksTransaction {
    let mut tx_auth = keychain.get_transaction_auth().unwrap();
    tx_auth.set_origin_nonce(nonce);

    let version = if is_mainnet {
        TransactionVersion::Mainnet
    } else {
        TransactionVersion::Testnet
    };
    let mut tx = StacksTransaction::new(
        version,
        tx_auth,
        TransactionPayload::Coinbase(CoinbasePayload([0u8; 32])),
    );
    tx.chain_id = chain_id;
    tx.anchor_mode = TransactionAnchorMode::OnChainOnly;
    let mut tx_signer = StacksTransactionSigner::new(&tx);
    keychain.sign_as_origin(&mut tx_signer);

    tx_signer.get_tx().unwrap()
}

fn inner_generate_poison_microblock_tx(
    keychain: &mut Keychain,
    nonce: u64,
    poison_payload: TransactionPayload,
    is_mainnet: bool,
    chain_id: u32,
) -> StacksTransaction {
    let mut tx_auth = keychain.get_transaction_auth().unwrap();
    tx_auth.set_origin_nonce(nonce);

    let version = if is_mainnet {
        TransactionVersion::Mainnet
    } else {
        TransactionVersion::Testnet
    };
    let mut tx = StacksTransaction::new(version, tx_auth, poison_payload);
    tx.chain_id = chain_id;
    tx.anchor_mode = TransactionAnchorMode::OnChainOnly;
    let mut tx_signer = StacksTransactionSigner::new(&tx);
    keychain.sign_as_origin(&mut tx_signer);

    tx_signer.get_tx().unwrap()
}

/// Constructs and returns a LeaderKeyRegisterOp out of the provided params
fn inner_generate_leader_key_register_op(
    address: StacksAddress,
    vrf_public_key: VRFPublicKey,
    consensus_hash: &ConsensusHash,
) -> BlockstackOperationType {
    BlockstackOperationType::LeaderKeyRegister(LeaderKeyRegisterOp {
        public_key: vrf_public_key,
        memo: vec![],
        address,
        consensus_hash: consensus_hash.clone(),
        vtxindex: 0,
        txid: Txid([0u8; 32]),
        block_height: 0,
        burn_header_hash: BurnchainHeaderHash::zero(),
    })
}

fn rotate_vrf_and_register(
    is_mainnet: bool,
    keychain: &mut Keychain,
    burn_block: &BlockSnapshot,
    btc_controller: &mut BitcoinRegtestController,
) -> Option<(Sha256dHash, VRFPublicKey)> {
    let vrf_public_key = keychain.rotate_vrf_keypair(burn_block.block_height);

    let vrf_pk = vrf_public_key.clone();

    let burnchain_tip_consensus_hash = &burn_block.consensus_hash;
    let op = inner_generate_leader_key_register_op(
        keychain.get_address(is_mainnet),
        vrf_pk,
        burnchain_tip_consensus_hash,
    );

    let mut one_off_signer = keychain.generate_op_signer();
    let txid_opt = btc_controller.submit_operation(op, &mut one_off_signer, 1);
    if txid_opt.is_some() {
        let txid = txid_opt.unwrap();
        let registered_key = RegistrationKey {
            block_height: burn_block.block_height + 1,
            op_vtxindex: 0,
            vrf_public_key: vrf_pk.clone(),
            vrf_secret_key: keychain.get_sk(&vrf_pk).unwrap().clone(),
            txid,
        };
        let registered_key_json = serde_json::to_string(&registered_key).unwrap();
        info!("writing vrf_key_prov.json: {:?}, add the transaction index and rename to vrf_key.json to activate", registered_key_json);
        // TODO(psq): the op_vtxindex will need to be manually adjusted once the tx has confirmed, or could be retrieved using the txid
        fs::write("vrf_key_prov.json", registered_key_json).expect("Unable to write key file file");

        return Some((txid, vrf_public_key));      
    }
    None
}

/// Constructs and returns a LeaderBlockCommitOp out of the provided params
fn inner_generate_block_commit_op(
    sender: BurnchainSigner,
    block_header_hash: BlockHeaderHash,
    burn_fee: u64,
    key: &RegisteredKey,
    parent_burnchain_height: u32,
    parent_winning_vtx: u16,
    vrf_seed: VRFSeed,
    commit_outs: Vec<StacksAddress>,
    sunset_burn: u64,
    current_burn_height: u64,
) -> BlockstackOperationType {
    let (parent_block_ptr, parent_vtxindex) = (parent_burnchain_height, parent_winning_vtx);
    let burn_parent_modulus = (current_burn_height % BURN_BLOCK_MINED_AT_MODULUS) as u8;

    BlockstackOperationType::LeaderBlockCommit(LeaderBlockCommitOp {
        sunset_burn,
        block_header_hash,
        burn_fee,
        input: (Txid([0; 32]), 0),
        apparent_sender: sender,
        key_block_ptr: key.block_height as u32,
        key_vtxindex: key.op_vtxindex as u16,
        memo: vec![],
        new_seed: vrf_seed,
        parent_block_ptr,
        parent_vtxindex,
        vtxindex: 0,
        txid: Txid([0u8; 32]),
        block_height: 0,
        burn_header_hash: BurnchainHeaderHash::zero(),
        burn_parent_modulus,
        commit_outs,
    })
}

/// Mine and broadcast a single microblock, unconditionally.
fn mine_one_microblock(
    microblock_state: &mut MicroblockMinerState,
    sortdb: &SortitionDB,
    chainstate: &mut StacksChainState,
    mempool: &MemPoolDB,
) -> Result<StacksMicroblock, ChainstateError> {
    debug!(
        "Try to mine one microblock off of {}/{} (at seq {})",
        &microblock_state.parent_consensus_hash,
        &microblock_state.parent_block_hash,
        chainstate
            .unconfirmed_state
            .as_ref()
            .map(|us| us.last_mblock_seq)
            .unwrap_or(0)
    );
    let mint_result = {
        let ic = sortdb.index_conn();
        let mut microblock_miner =
            match StacksMicroblockBuilder::resume_unconfirmed(chainstate, &ic) {
                Ok(x) => x,
                Err(e) => {
                    let msg = format!(
                        "Failed to create a microblock miner at chaintip {}/{}: {:?}",
                        &microblock_state.parent_consensus_hash,
                        &microblock_state.parent_block_hash,
                        &e
                    );
                    error!("{}", msg);
                    return Err(e);
                }
            };

        let mblock = microblock_miner.mine_next_microblock(mempool, &microblock_state.miner_key)?;

        info!("Mined microblock with {} transactions", mblock.txs.len());

        Ok(mblock)
    };

    let mined_microblock = match mint_result {
        Ok(mined_microblock) => mined_microblock,
        Err(e) => {
            warn!("Failed to mine microblock: {}", e);
            return Err(e);
        }
    };

    // preprocess the microblock locally
    chainstate.preprocess_streamed_microblock(
        &microblock_state.parent_consensus_hash,
        &microblock_state.parent_block_hash,
        &mined_microblock,
    )?;

    microblock_state.quantity += 1;
    return Ok(mined_microblock);
}

fn try_mine_microblock(
    config: &Config,
    microblock_miner_state: &mut Option<MicroblockMinerState>,
    chainstate: &mut StacksChainState,
    sortdb: &SortitionDB,
    mem_pool: &MemPoolDB,
    winning_tip_opt: Option<&(ConsensusHash, BlockHeaderHash, Secp256k1PrivateKey)>,
) -> Result<Option<StacksMicroblock>, NetError> {
    let mut next_microblock = None;
    if microblock_miner_state.is_none() {
        // are we the current sortition winner?  Do we need to instantiate?
        if let Some((ch, bhh, microblock_privkey)) = winning_tip_opt.as_ref() {
            debug!(
                "Instantiate microblock mining state off of {}/{}",
                &ch, &bhh
            );
            // we won a block! proceed to build a microblock tail if we've stored it
            match StacksChainState::get_anchored_block_header_info(chainstate.db(), ch, bhh) {
                Ok(Some(_)) => {
                    microblock_miner_state.replace(MicroblockMinerState {
                        parent_consensus_hash: ch.clone(),
                        parent_block_hash: bhh.clone(),
                        miner_key: microblock_privkey.clone(),
                        frequency: config.node.microblock_frequency,
                        last_mined: 0,
                        quantity: 0,
                    });
                }
                Ok(None) => {
                    warn!(
                        "No such anchored block: {}/{}.  Cannot mine microblocks",
                        ch, bhh
                    );
                }
                Err(e) => {
                    warn!(
                        "Failed to get anchored block cost for {}/{}: {:?}",
                        ch, bhh, &e
                    );
                }
            }
        }
    }

    if let Some((ch, bhh, ..)) = winning_tip_opt.as_ref() {
        if let Some(mut microblock_miner) = microblock_miner_state.take() {
            if microblock_miner.parent_consensus_hash == *ch
                && microblock_miner.parent_block_hash == *bhh
            {
                if microblock_miner.last_mined + (microblock_miner.frequency as u128)
                    < get_epoch_time_ms()
                {
                    // opportunistically try and mine, but only if there's no attachable blocks
                    let num_attachable =
                        StacksChainState::count_attachable_staging_blocks(chainstate.db(), 1, 0)?;
                    if num_attachable == 0 {
                        match mine_one_microblock(
                            &mut microblock_miner,
                            sortdb,
                            chainstate,
                            &mem_pool,
                        ) {
                            Ok(microblock) => {
                                // will need to relay this
                                next_microblock = Some(microblock);
                            }
                            Err(ChainstateError::NoTransactionsToMine) => {
                                info!("Will keep polling mempool for transactions to include in a microblock");
                            }
                            Err(e) => {
                                warn!("Failed to mine one microblock: {:?}", &e);
                            }
                        }
                    }
                }
                microblock_miner.last_mined = get_epoch_time_ms();
                microblock_miner_state.replace(microblock_miner);
            }
            // otherwise, we're not the sortition winner, and the microblock miner state can be
            // discarded.
        }
    } else {
        // no longer a winning tip
        let _ = microblock_miner_state.take();
    }

    Ok(next_microblock)
}

fn run_microblock_tenure(
    config: &Config,
    microblock_miner_state: &mut Option<MicroblockMinerState>,
    chainstate: &mut StacksChainState,
    sortdb: &mut SortitionDB,
    mem_pool: &MemPoolDB,
    relayer: &mut Relayer,
    miner_tip: Option<&(ConsensusHash, BlockHeaderHash, Secp256k1PrivateKey)>,
) {
    // TODO: this is sensitive to poll latency -- can we call this on a fixed
    // schedule, regardless of network activity?
    if let Some((ref parent_consensus_hash, ref parent_block_hash, _)) = miner_tip.as_ref() {
        debug!(
            "Run microblock tenure for {}/{}",
            parent_consensus_hash, parent_block_hash
        );

        // Mine microblocks, if we're active
        let next_microblock_opt = match try_mine_microblock(
            &config,
            microblock_miner_state,
            chainstate,
            sortdb,
            mem_pool,
            miner_tip.clone(),
        ) {
            Ok(x) => x,
            Err(e) => {
                warn!("Failed to mine next microblock: {:?}", &e);
                None
            }
        };

        // did we mine anything?
        if let Some(next_microblock) = next_microblock_opt {
            // apply it
            Relayer::refresh_unconfirmed(chainstate, sortdb);

            // send it off
            let microblock_hash = next_microblock.block_hash();
            if let Err(e) = relayer.broadcast_microblock(
                parent_consensus_hash,
                parent_block_hash,
                next_microblock,
            ) {
                error!(
                    "Failure trying to broadcast microblock {}: {}",
                    microblock_hash, e
                );
            }
        }
    }
}

/// Grant the p2p thread a copy of the unconfirmed microblock transaction list, so it can serve it
/// out via the unconfirmed transaction API.
/// Not the prettiest way to do this, but the least disruptive way to do this.
fn send_unconfirmed_txs(
    chainstate: &StacksChainState,
    unconfirmed_txs: Arc<Mutex<UnconfirmedTxMap>>,
) {
    if let Some(ref unconfirmed) = chainstate.unconfirmed_state {
        match unconfirmed_txs.lock() {
            Ok(mut txs) => {
                txs.clear();
                txs.extend(unconfirmed.mined_txs.clone());
            }
            Err(e) => {
                // can only happen due to a thread panic in the relayer
                error!("FATAL: unconfirmed tx arc mutex is poisoned: {:?}", &e);
                panic!();
            }
        };
    }
}

/// Have the p2p thread receive unconfirmed txs
fn recv_unconfirmed_txs(
    chainstate: &mut StacksChainState,
    unconfirmed_txs: Arc<Mutex<UnconfirmedTxMap>>,
) {
    if let Some(ref mut unconfirmed) = chainstate.unconfirmed_state {
        match unconfirmed_txs.lock() {
            Ok(txs) => {
                unconfirmed.mined_txs.clear();
                unconfirmed.mined_txs.extend(txs.clone());
            }
            Err(e) => {
                // can only happen due to a thread panic in the relayer
                error!("FATAL: unconfirmed arc mutex is poisoned: {:?}", &e);
                panic!();
            }
        };
    }
}

fn spawn_peer(
    is_mainnet: bool,
    mut this: PeerNetwork,
    p2p_sock: &SocketAddr,
    rpc_sock: &SocketAddr,
    config: Config,
    poll_timeout: u64,
    relay_channel: SyncSender<RelayerDirective>,
    mut sync_comms: PoxSyncWatchdogComms,
    attachments_rx: Receiver<HashSet<AttachmentInstance>>,
    unconfirmed_txs: Arc<Mutex<UnconfirmedTxMap>>,
) -> Result<JoinHandle<()>, NetError> {
    let burn_db_path = config.get_burn_db_file_path();
    let stacks_chainstate_path = config.get_chainstate_path();
    let block_limit = config.block_limit.clone();
    let exit_at_block_height = config.burnchain.process_exit_at_block_height;

    this.bind(p2p_sock, rpc_sock).unwrap();
    let (mut dns_resolver, mut dns_client) = DNSResolver::new(10);
    let sortdb = SortitionDB::open(&burn_db_path, false).map_err(NetError::DBError)?;

    let (mut chainstate, _) = StacksChainState::open_with_block_limit(
        is_mainnet,
        config.burnchain.chain_id,
        &stacks_chainstate_path,
        block_limit,
    )
    .map_err(|e| NetError::ChainstateError(e.to_string()))?;

    let mut mem_pool = MemPoolDB::open(
        is_mainnet,
        config.burnchain.chain_id,
        &stacks_chainstate_path,
    )
    .map_err(NetError::DBError)?;

    // buffer up blocks to store without stalling the p2p thread
    let mut results_with_data = VecDeque::new();

    let server_thread = thread::Builder::new()
        .name("p2p".to_string())
        .spawn(move || {
            let handler_args = RPCHandlerArgs {
                exit_at_block_height: exit_at_block_height.as_ref(),
                genesis_chainstate_hash: Sha256Sum::from_hex(stx_genesis::GENESIS_CHAINSTATE_HASH)
                    .unwrap(),
                ..RPCHandlerArgs::default()
            };

            let mut disconnected = false;
            let mut num_p2p_state_machine_passes = 0;
            let mut num_inv_sync_passes = 0;
            let mut mblock_deadline = 0;

            while !disconnected {
                let download_backpressure = results_with_data.len() > 0;
                let poll_ms = if !download_backpressure && this.has_more_downloads() {
                    // keep getting those blocks -- drive the downloader state-machine
                    debug!(
                        "P2P: backpressure: {}, more downloads: {}",
                        download_backpressure,
                        this.has_more_downloads()
                    );
                    100
                } else {
                    cmp::min(poll_timeout, config.node.microblock_frequency)
                };

                let mut expected_attachments = match attachments_rx.try_recv() {
                    Ok(expected_attachments) => expected_attachments,
                    _ => {
                        debug!("Atlas: attachment channel is empty");
                        HashSet::new()
                    }
                };

                let _ = Relayer::setup_unconfirmed_state_readonly(&mut chainstate, &sortdb);
                recv_unconfirmed_txs(&mut chainstate, unconfirmed_txs.clone());

                match this.run(
                    &sortdb,
                    &mut chainstate,
                    &mut mem_pool,
                    Some(&mut dns_client),
                    download_backpressure,
                    poll_ms,
                    &handler_args,
                    &mut expected_attachments,
                ) {
                    Ok(network_result) => {
                        if num_p2p_state_machine_passes < network_result.num_state_machine_passes {
                            // p2p state-machine did a full pass. Notify anyone listening.
                            sync_comms.notify_p2p_state_pass();
                            num_p2p_state_machine_passes = network_result.num_state_machine_passes;
                        }

                        if num_inv_sync_passes < network_result.num_inv_sync_passes {
                            // inv-sync state-machine did a full pass. Notify anyone listening.
                            sync_comms.notify_inv_sync_pass();
                            num_inv_sync_passes = network_result.num_inv_sync_passes;
                        }

                        if network_result.has_data_to_store() {
                            results_with_data
                                .push_back(RelayerDirective::HandleNetResult(network_result));
                        }

                        // only do this on the Ok() path, even if we're mining, because an error in
                        // network dispatching is likely due to resource exhaustion
                        if mblock_deadline < get_epoch_time_ms() {
                            results_with_data.push_back(RelayerDirective::RunMicroblockTenure);
                            mblock_deadline =
                                get_epoch_time_ms() + (config.node.microblock_frequency as u128);
                        }
                    }
                    Err(e) => {
                        error!("P2P: Failed to process network dispatch: {:?}", &e);
                        if config.is_node_event_driven() {
                            panic!();
                        }
                    }
                };

                while let Some(next_result) = results_with_data.pop_front() {
                    // have blocks, microblocks, and/or transactions (don't care about anything else),
                    // or a directive to mine microblocks
                    if let Err(e) = relay_channel.try_send(next_result) {
                        debug!(
                            "P2P: {:?}: download backpressure detected",
                            &this.local_peer
                        );
                        match e {
                            TrySendError::Full(directive) => {
                                if let RelayerDirective::RunMicroblockTenure = directive {
                                    // can drop this
                                } else {
                                    // don't lose this data -- just try it again
                                    results_with_data.push_front(directive);
                                }
                                break;
                            }
                            TrySendError::Disconnected(_) => {
                                info!("P2P: Relayer hang up with p2p channel");
                                disconnected = true;
                                break;
                            }
                        }
                    } else {
                        debug!("P2P: Dispatched result to Relayer!");
                    }
                }
            }
            debug!("P2P thread exit!");
        })
        .unwrap();

    let _jh = thread::Builder::new()
        .name("dns-resolver".to_string())
        .spawn(move || {
            dns_resolver.thread_main();
        })
        .unwrap();

    Ok(server_thread)
}

fn spawn_miner_relayer(
    is_mainnet: bool,
    chain_id: u32,
    mut relayer: Relayer,
    local_peer: LocalPeer,
    config: Config,
    mut keychain: Keychain,
    burn_db_path: String,
    stacks_chainstate_path: String,
    relay_channel: Receiver<RelayerDirective>,
    rpc_channel: Receiver<RPCDirective>,
    event_dispatcher: EventDispatcher,
    blocks_processed: BlocksProcessedCounter,
    burnchain: Burnchain,
    coord_comms: CoordinatorChannels,
    unconfirmed_txs: Arc<Mutex<UnconfirmedTxMap>>,
) -> Result<(), NetError> {
    // Note: the relayer is *the* block processor, it is responsible for writes to the chainstate --
    //   no other codepaths should be writing once this is spawned.
    //
    // the relayer _should not_ be modifying the sortdb,
    //   however, it needs a mut reference to create read TXs.
    //   should address via #1449
    let mut sortdb = SortitionDB::open(&burn_db_path, true).map_err(NetError::DBError)?;

    let (mut chainstate, _) = StacksChainState::open_with_block_limit(
        is_mainnet,
        chain_id,
        &stacks_chainstate_path,
        config.block_limit.clone(),
    )
    .map_err(|e| NetError::ChainstateError(e.to_string()))?;

    let mut mem_pool = MemPoolDB::open(is_mainnet, chain_id, &stacks_chainstate_path)
        .map_err(NetError::DBError)?;

    let mut last_mined_blocks: HashMap<
        BurnchainHeaderHash,
        Vec<(AssembledAnchorBlock, Secp256k1PrivateKey)>,
    > = HashMap::new();
    let burn_fee_cap = config.burnchain.burn_fee_cap;

    let mut bitcoin_controller = BitcoinRegtestController::new_dummy(config.clone());
    let mut microblock_miner_state = None;
    let mut miner_tip = None;
    let mut last_microblock_tenure_time = 0;

    let _relayer_handle = thread::Builder::new().name("relayer".to_string()).spawn(move || {

        // TODO(psq): use both rpc_channel and relay_channel with try_rcv (and sleep in between?)
        // an other use case for crossbeam_channel?

        loop {
            match relay_channel.try_recv() {
                Ok(mut directive) => {
                    match directive {
                        RelayerDirective::HandleNetResult(ref mut net_result) => {
                            debug!("Relayer: Handle network result");
                            let net_receipts = relayer
                                .process_network_result(
                                    &local_peer,
                                    net_result,
                                    &mut sortdb,
                                    &mut chainstate,
                                    &mut mem_pool,
                                    Some(&coord_comms),
                                )
                                .expect("BUG: failure processing network results");

                            let mempool_txs_added = net_receipts.mempool_txs_added.len();
                            if mempool_txs_added > 0 {
                                event_dispatcher.process_new_mempool_txs(net_receipts.mempool_txs_added);
                            }

                            // Dispatch retrieved attachments, if any.
                            if net_result.has_attachments() {
                                event_dispatcher.process_new_attachments(&net_result.attachments);
                            }

                            // synchronize unconfirmed tx index to p2p thread
                            send_unconfirmed_txs(&chainstate, unconfirmed_txs.clone());
                        }
                        RelayerDirective::ProcessTenure(consensus_hash, burn_hash, block_header_hash) => {
                            debug!(
                                "Relayer: Process tenure {}/{} in {}",
                                &consensus_hash, &block_header_hash, &burn_hash
                            );
                            if let Some(last_mined_blocks_at_burn_hash) =
                                last_mined_blocks.remove(&burn_hash)
                            {
                                for (last_mined_block, microblock_privkey) in
                                    last_mined_blocks_at_burn_hash.into_iter()
                                {
                                    let AssembledAnchorBlock {
                                        parent_consensus_hash,
                                        anchored_block: mined_block,
                                        my_burn_hash: mined_burn_hash,
                                        attempt: _,
                                    } = last_mined_block;
                                    if mined_block.block_hash() == block_header_hash
                                        && burn_hash == mined_burn_hash
                                    {
                                        // we won!
                                        let reward_block_height = mined_block.header.total_work.work + MINER_REWARD_MATURITY;
                                        info!("Won sortition! Mining reward will be received in {} blocks (block #{})", MINER_REWARD_MATURITY, reward_block_height);
                                        debug!("Won sortition!";
                                              "stacks_header" => %block_header_hash,
                                              "burn_hash" => %mined_burn_hash,
                                        );

                                        increment_stx_blocks_mined_counter();

                                        match inner_process_tenure(
                                            &mined_block,
                                            &consensus_hash,
                                            &parent_consensus_hash,
                                            &mut sortdb,
                                            &mut chainstate,
                                            &coord_comms,
                                        ) {
                                            Ok(coordinator_running) => {
                                                if !coordinator_running {
                                                    warn!(
                                                        "Coordinator stopped, stopping relayer thread..."
                                                    );
                                                    return;
                                                }
                                            }
                                            Err(e) => {
                                                warn!(
                                                    "Error processing my tenure, bad block produced: {}",
                                                    e
                                                );
                                                warn!(
                                                    "Bad block";
                                                    "stacks_header" => %block_header_hash,
                                                    "data" => %to_hex(&mined_block.serialize_to_vec()),
                                                );
                                                continue;
                                            }
                                        };

                                        // advertize _and_ push blocks for now
                                        let blocks_available = Relayer::load_blocks_available_data(
                                            &sortdb,
                                            vec![consensus_hash.clone()],
                                        )
                                        .expect("Failed to obtain block information for a block we mined.");
                                        if let Err(e) = relayer.advertize_blocks(blocks_available) {
                                            warn!("Failed to advertise new block: {}", e);
                                        }

                                        let snapshot = SortitionDB::get_block_snapshot_consensus(
                                            sortdb.conn(),
                                            &consensus_hash,
                                        )
                                        .expect("Failed to obtain snapshot for block")
                                        .expect("Failed to obtain snapshot for block");
                                        if !snapshot.pox_valid {
                                            warn!(
                                                "Snapshot for {} is no longer valid; discarding {}...",
                                                &consensus_hash,
                                                &mined_block.block_hash()
                                            );
                                        } else {
                                            let ch = snapshot.consensus_hash.clone();
                                            let bh = mined_block.block_hash();

                                            if let Err(e) = relayer
                                                .broadcast_block(snapshot.consensus_hash, mined_block)
                                            {
                                                warn!("Failed to push new block: {}", e);
                                            }

                                            // proceed to mine microblocks
                                            debug!(
                                                "Microblock miner tip is now {}/{}",
                                                &consensus_hash, &block_header_hash
                                            );
                                            miner_tip = Some((ch, bh, microblock_privkey));
                                        }
                                    } else {
                                        debug!("Did not win sortition, my blocks [burn_hash= {}, block_hash= {}], their blocks [parent_consenus_hash= {}, burn_hash= {}, block_hash ={}]",
                                          mined_burn_hash, mined_block.block_hash(), parent_consensus_hash, burn_hash, block_header_hash);

                                        miner_tip = None;
                                    }
                                }
                            }
                        }
                        RelayerDirective::RunTenure(registered_key, last_burn_block) => {
                            let burn_header_hash = last_burn_block.burn_header_hash.clone();
                            debug!(
                                "Relayer: Run tenure";
                                "height" => last_burn_block.block_height,
                                "burn_header_hash" => %burn_header_hash
                            );
                            let mut last_mined_blocks_vec = last_mined_blocks
                                .remove(&burn_header_hash)
                                .unwrap_or_default();

                            let last_mined_block_opt = InitializedNeonNode::relayer_run_tenure(
                                &config,
                                registered_key,
                                &mut chainstate,
                                &mut sortdb,
                                &burnchain,
                                last_burn_block,
                                &mut keychain,
                                &mut mem_pool,
                                burn_fee_cap,
                                &mut bitcoin_controller,
                                &last_mined_blocks_vec.iter().map(|(blk, _)| blk).collect(),
                            );
                            if let Some((last_mined_block, microblock_privkey)) = last_mined_block_opt {
                                if last_mined_blocks_vec.len() == 0 {
                                    // (for testing) only bump once per epoch
                                    bump_processed_counter(&blocks_processed);
                                }
                                last_mined_blocks_vec.push((last_mined_block, microblock_privkey));
                            }
                            last_mined_blocks.insert(burn_header_hash, last_mined_blocks_vec);
                        }
                        RelayerDirective::RegisterKey(ref last_burn_block) => {
                            rotate_vrf_and_register(
                                is_mainnet,
                                &mut keychain,
                                last_burn_block,
                                &mut bitcoin_controller,
                            );
                            bump_processed_counter(&blocks_processed);
                        }
                        RelayerDirective::RunMicroblockTenure => {
                            if last_microblock_tenure_time + (config.node.microblock_frequency as u128) > get_epoch_time_ms() {
                                // only mine when necessary -- the deadline to begin hasn't passed yet
                                continue;
                            }
                            last_microblock_tenure_time = get_epoch_time_ms();

                            debug!("Relayer: run microblock tenure");

                            // unconfirmed state must be consistent with the chain tip
                            if miner_tip.is_some() {
                                Relayer::refresh_unconfirmed(&mut chainstate, &mut sortdb);
                            }

                            run_microblock_tenure(
                                &config,
                                &mut microblock_miner_state,
                                &mut chainstate,
                                &mut sortdb,
                                &mem_pool,
                                &mut relayer,
                                miner_tip.as_ref(),
                            );

                            // synchronize unconfirmed tx index to p2p thread
                            send_unconfirmed_txs(&chainstate, unconfirmed_txs.clone());
                        }
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    break;
                }
                Err(TryRecvError::Empty) => {
                    // nothing to do, try next channel
                }
            };
            match rpc_channel.try_recv() {
                Ok(directive) => {
                    match directive {
                        RPCDirective::RegisterKey(
                            consensus_hash,    // ConsensusHash,
                            // burn_header_hash,  // BurnchainHeaderHash, TODO(psq): not use as would require height to retrieve uniquely with sortitoin db
                            tx,
                        ) => {
                            let snapshot_res = SortitionDB::get_block_snapshot_consensus(
                                sortdb.conn(),
                                &consensus_hash,
                            );
                            if snapshot_res.is_ok() && snapshot_res.as_ref().unwrap().is_some() {
                                let snapshot = snapshot_res.unwrap().unwrap();
                                match rotate_vrf_and_register(
                                    is_mainnet,
                                    &mut keychain,
                                    &snapshot,
                                    &mut bitcoin_controller,
                                ) {
                                    Some((txid, vrf_public_key)) => {
                                        let response = RegisterKeyResponse {
                                            vrf_public_key,
                                            txid,
                                        };
                                        if let Err(err) = tx.send(RegisterKeyRPCResponseOk { response }) {
                                            error!("Error sending back RegisterKey response {:?}", err);
                                        }
                                    },
                                    None => {
                                        if let Err(err) = tx.send(RegisterKeyRPCResponseErr { err: ChainstateError::InvalidProof }) {
                                            error!("Error sending back RegisterKey response {:?}", err);
                                        }
                                    }
                                }
                            } else {
                                if let Err(err) = tx.send(RegisterKeyRPCResponseErr { err: ChainstateError::NoSuchBlockError }) {
                                    error!("Error sending back RegisterKey response {:?}", err);
                                }
                            }
                        }
                        RPCDirective::PrepareBlock(
                            vrf_pk,
                            anchored_block_hash,
                            parent_consensus_hash,
                            parent_block_burn_height,
                            parent_winning_vtxindex,
                            target_burn_block,
                            txids,
                            tx,
                        ) => {
                            info!("Relayer: PrepareBlock {:?} / {:?} previous winner {:?}-{:?}, target {:?}", anchored_block_hash, parent_consensus_hash, parent_block_burn_height, parent_winning_vtxindex, target_burn_block);

                            match InitializedNeonNode::relayer_prepare_block(
                                &mut chainstate,
                                &mut sortdb,
                                &burnchain,
                                &mut mem_pool,
                                &mut keychain,

                                vrf_pk,
                                anchored_block_hash,
                                parent_consensus_hash,
                                parent_block_burn_height,
                                parent_winning_vtxindex,
                                target_burn_block,
                                txids,
                            ) {
                                Ok((last_mined_block, seed, recipients, microblock_privkey)) => {
                                    let mut last_mined_blocks_vec = last_mined_blocks
                                        .remove(&last_mined_block.my_burn_hash)
                                        .unwrap_or_default();

                                    // if last_mined_blocks_vec.len() == 0 {
                                    //     // (for testing) only bump once per epoch
                                    //     bump_processed_counter(&blocks_processed);
                                    // }

                                    let my_burn_hash = last_mined_block.my_burn_hash;
                                    let block_hash = last_mined_block.anchored_block.block_hash();
                                    last_mined_blocks_vec.push((last_mined_block, microblock_privkey));
                                    last_mined_blocks.insert(my_burn_hash, last_mined_blocks_vec);

                                    // TODO(psq): create option (either response, or error)
                                    let response = BuildBlockTemplateResponse {
                                        block_hash: block_hash,
                                        new_seed: seed,
                                        recipients,
                                    };
                                    if let Err(err) = tx.send(BuildBlockTemplateRPCResponseOk { response }) {
                                        error!("Error sending back PrepareBlock response {:?}", err);
                                    }
                                }
                                Err(err) => {
                                    if let Err(err) = tx.send(BuildBlockTemplateRPCResponseErr { err }) {
                                        error!("Error sending back PrepareBlock response {:?}", err);
                                    }
                                }
                            }
                        }
                        RPCDirective::StoreMinerBlock(
                            parent_consensus_hash,
                            my_burn_hash,
                            anchored_block,
                            microblock_secret_key,
                        ) => {
                            debug!("Relayer: Store Miner Block {:?} / {:?} for burn {:?}", anchored_block.header.parent_block, anchored_block.block_hash(), my_burn_hash);
                            let assembled_block = AssembledAnchorBlock {
                                parent_consensus_hash,
                                my_burn_hash,
                                anchored_block,
                                attempt: 0,
                            };
                            // store block to be used if block is a winner for next sortition

                            let mut last_mined_blocks_vec = last_mined_blocks
                                .remove(&my_burn_hash)
                                .unwrap_or_default();
                            debug!("last_mined_blocks_vec {:#?}", last_mined_blocks_vec);
                            last_mined_blocks_vec.push((assembled_block,  microblock_secret_key));
                            last_mined_blocks.insert(my_burn_hash, last_mined_blocks_vec);
                            // let last_mined_blocks_vec = vec![(assembled_block,  microblock_secret_key)];
                            // last_mined_blocks.insert(my_burn_hash, last_mined_blocks_vec);
                            debug!("StoreMinerBlock.last_mined_blocks {:?} => {:#?}", my_burn_hash, last_mined_blocks);
                        }
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    break;
                }
                Err(TryRecvError::Empty) => {
                    // nothing to do, try next channel
                }
            }
            thread::sleep(std::time::Duration::from_millis(100));  // TODO(psq): too long? too short? make it configurable? or go to once per second?
        }
        debug!("Relayer exit!");
    }).unwrap();

    Ok(())
}

enum LeaderKeyRegistrationState {
    Inactive,
    Pending,
    Active(RegisteredKey),
}

impl InitializedNeonNode {
    fn new(
        config: Config,
        keychain: Keychain,
        event_dispatcher: EventDispatcher,
        last_burn_block: Option<BurnchainTip>,
        miner: bool,
        blocks_processed: BlocksProcessedCounter,
        coord_comms: CoordinatorChannels,
        sync_comms: PoxSyncWatchdogComms,
        burnchain: Burnchain,
        attachments_rx: Receiver<HashSet<AttachmentInstance>>,
        atlas_config: AtlasConfig,
        leader_key_registration_state: LeaderKeyRegistrationState,
    ) -> InitializedNeonNode {
        // we can call _open_ here rather than _connect_, since connect is first called in
        //   make_genesis_block
        let sortdb = SortitionDB::open(&config.get_burn_db_file_path(), false)
            .expect("Error while instantiating sortition db");

        let view = {
            let sortition_tip = SortitionDB::get_canonical_burn_chain_tip(&sortdb.conn())
                .expect("Failed to get sortition tip");
            SortitionDB::get_burnchain_view(&sortdb.conn(), &burnchain, &sortition_tip).unwrap()
        };

        // create a new peerdb
        let data_url = UrlString::try_from(format!("{}", &config.node.data_url)).unwrap();
        let initial_neighbors = config.node.bootstrap_node.clone();
        if initial_neighbors.len() > 0 {
            info!(
                "Will bootstrap from peers {}",
                VecDisplay(&initial_neighbors)
            );
        } else {
            warn!("Without a peer to bootstrap from, the node will start mining a new chain");
        }

        let p2p_sock: SocketAddr = config.node.p2p_bind.parse().expect(&format!(
            "Failed to parse socket: {}",
            &config.node.p2p_bind
        ));
        let rpc_sock = config.node.rpc_bind.parse().expect(&format!(
            "Failed to parse socket: {}",
            &config.node.rpc_bind
        ));
        let p2p_addr: SocketAddr = config.node.p2p_address.parse().expect(&format!(
            "Failed to parse socket: {}",
            &config.node.p2p_address
        ));
        let node_privkey = {
            let mut re_hashed_seed = config.node.local_peer_seed.clone();
            let my_private_key = loop {
                match Secp256k1PrivateKey::from_slice(&re_hashed_seed[..]) {
                    Ok(sk) => break sk,
                    Err(_) => {
                        re_hashed_seed = Sha256Sum::from_data(&re_hashed_seed[..])
                            .as_bytes()
                            .to_vec()
                    }
                }
            };
            my_private_key
        };

        let mut peerdb = PeerDB::connect(
            &config.get_peer_db_path(),
            true,
            config.burnchain.chain_id,
            burnchain.network_id,
            Some(node_privkey),
            config.connection_options.private_key_lifetime.clone(),
            PeerAddress::from_socketaddr(&p2p_addr),
            p2p_sock.port(),
            data_url,
            &vec![],
            Some(&initial_neighbors),
        )
        .map_err(|e| {
            eprintln!("Failed to open {}: {:?}", &config.get_peer_db_path(), &e);
            panic!();
        })
        .unwrap();

        {
            // bootstrap nodes *always* allowed
            let mut tx = peerdb.tx_begin().unwrap();
            for initial_neighbor in initial_neighbors.iter() {
                PeerDB::set_allow_peer(
                    &mut tx,
                    initial_neighbor.addr.network_id,
                    &initial_neighbor.addr.addrbytes,
                    initial_neighbor.addr.port,
                    -1,
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }

        if !config.node.deny_nodes.is_empty() {
            warn!("Will ignore nodes {:?}", &config.node.deny_nodes);
        }

        {
            let mut tx = peerdb.tx_begin().unwrap();
            for denied in config.node.deny_nodes.iter() {
                PeerDB::set_deny_peer(
                    &mut tx,
                    denied.addr.network_id,
                    &denied.addr.addrbytes,
                    denied.addr.port,
                    get_epoch_time_secs() + 24 * 365 * 3600,
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }
        let atlasdb = AtlasDB::connect(atlas_config, &config.get_atlas_db_path(), true).unwrap();

        let local_peer = match PeerDB::get_local_peer(peerdb.conn()) {
            Ok(local_peer) => local_peer,
            _ => panic!("Unable to retrieve local peer"),
        };

        let (rpc_send, rpc_recv) = sync_channel(RPC_MAX_BUFFER);

        // now we're ready to instantiate a p2p network object, the relayer, and the event dispatcher
        let mut p2p_net = PeerNetwork::new(
            peerdb,
            atlasdb,
            local_peer.clone(),
            config.burnchain.peer_version,
            burnchain.clone(),
            view,
            config.connection_options.clone(),
            Some(rpc_send),
        );

        // setup the relayer channel
        let (relay_send, relay_recv) = sync_channel(RELAYER_MAX_BUFFER);

        let burnchain_signer = keychain.get_burnchain_signer();
        let relayer = Relayer::from_p2p(&mut p2p_net);
        let shared_unconfirmed_txs = Arc::new(Mutex::new(UnconfirmedTxMap::new()));

        let sleep_before_tenure = config.node.wait_time_for_microblocks;
        spawn_miner_relayer(
            config.is_mainnet(),
            config.burnchain.chain_id,
            relayer,
            local_peer,
            config.clone(),
            keychain,
            config.get_burn_db_file_path(),
            config.get_chainstate_path(),
            relay_recv,
            rpc_recv,
            event_dispatcher,
            blocks_processed.clone(),
            burnchain,
            coord_comms,
            shared_unconfirmed_txs.clone(),
        )
        .expect("Failed to initialize mine/relay thread");

        spawn_peer(
            config.is_mainnet(),
            p2p_net,
            &p2p_sock,
            &rpc_sock,
            config.clone(),
            5000,
            relay_send.clone(),
            sync_comms,
            attachments_rx,
            shared_unconfirmed_txs,
        )
        .expect("Failed to initialize mine/relay thread");

        info!("Start HTTP server on: {}", &config.node.rpc_bind);
        info!("Start P2P server on: {}", &config.node.p2p_bind);

        let last_burn_block = last_burn_block.map(|x| x.block_snapshot);

        let is_miner = miner;

        let atlas_config = AtlasConfig::default(config.is_mainnet());
        InitializedNeonNode {
            config: config,
            relay_channel: relay_send,
            last_burn_block,
            burnchain_signer,
            is_miner,
            sleep_before_tenure,
            atlas_config,
            leader_key_registration_state,
        }
    }

    /// Tell the relayer to fire off a tenure and a block commit op.
    pub fn relayer_issue_tenure(&mut self) -> bool {
        if !self.is_miner {
            // node is a follower, don't try to issue a tenure
            return true;
        }

        if let Some(burnchain_tip) = self.last_burn_block.clone() {
            match self.leader_key_registration_state {
                LeaderKeyRegistrationState::Active(ref key) => {
                    debug!("Using key {:?}", &key.vrf_public_key);
                    // sleep a little before building the anchor block, to give any broadcasted
                    //   microblocks time to propagate.
                    thread::sleep(std::time::Duration::from_millis(self.sleep_before_tenure));
                    self.relay_channel
                        .send(RelayerDirective::RunTenure(key.clone(), burnchain_tip))
                        .is_ok()
                }
                LeaderKeyRegistrationState::Inactive => {
                    warn!("Skipped tenure because no active VRF key. Trying to register one.");
                    self.leader_key_registration_state = LeaderKeyRegistrationState::Pending;
                    self.relay_channel
                        .send(RelayerDirective::RegisterKey(burnchain_tip))
                        .is_ok()
                }
                LeaderKeyRegistrationState::Pending => true,
            }
        } else {
            warn!("Do not know the last burn block. As a miner, this is bad.");
            true
        }
    }

    /// Notify the relayer of a sortition, telling it to process the block
    ///  and advertize it if it was mined by the node.
    /// returns _false_ if the relayer hung up the channel.
    pub fn relayer_sortition_notify(&self) -> bool {
        if !self.is_miner {
            // node is a follower, don't try to process my own tenure.
            return true;
        }

        if let Some(ref snapshot) = &self.last_burn_block {
            debug!(
                "Notify sortition! Last snapshot is {}/{} ({})",
                &snapshot.consensus_hash,
                &snapshot.burn_header_hash,
                &snapshot.winning_stacks_block_hash
            );
            if snapshot.sortition {
                return self
                    .relay_channel
                    .send(RelayerDirective::ProcessTenure(
                        snapshot.consensus_hash.clone(),
                        snapshot.parent_burn_header_hash.clone(),
                        snapshot.winning_stacks_block_hash.clone(),
                    ))
                    .is_ok();
            }
        } else {
            debug!("Notify sortition! No last burn block");
        }
        true
    }

    pub fn relayer_prepare_block(
        chainstate: &mut StacksChainState,
        burn_db: &mut SortitionDB,
        burnchain: &Burnchain,
        mempool: &mut MemPoolDB,
        keychain: &mut Keychain,

        vrf_pk: VRFPublicKey,
        anchored_block_hash: BlockHeaderHash,
        parent_consensus_hash: ConsensusHash,

        // TODO(psq): oops, not needed, remove from api
        // could we use instead of not have the node already processed that (not with consensus that requrires going deep?)
        _parent_block_burn_height: u64,
        _parent_winning_vtxindex: u16,

        target_burn_block: u64,      
        txids: Option<Vec<Txid>>,  // TODO(psq): not used yet, uses the default mempool behavior
    ) -> Result<(AssembledAnchorBlock, VRFSeed, Vec<StacksAddress>, Secp256k1PrivateKey), ChainstateError> {

        let mainnet = chainstate.config().mainnet;
        let chain_id = chainstate.config().chain_id;

        // // get the correct tip for anchored_block_hash
        // let tip_opt: Option<BlockSnapshot> = SortitionDB::get_block_snapshot_consensus(
        //     burn_db.conn(),
        //     &parent_consensus_hash,
        // ).unwrap();

        // TODO(psq): is this really necessary, just as a sanity check?
        if let Some(parent_snapshot) = SortitionDB::get_block_snapshot_consensus(burn_db.conn(), &parent_consensus_hash).unwrap() {
            let (tip_consensus_hash, tip_block_hash, tip_height) = (
                parent_snapshot.consensus_hash.clone(),
                parent_snapshot.winning_stacks_block_hash.clone(),
                parent_snapshot.block_height,
            );

            if tip_block_hash != anchored_block_hash {
                error!("Requested parent block hash mismatch {:?} != {:?}", tip_block_hash, anchored_block_hash);
                return Err(ChainstateError::NoSuchBlockError);
            }

            debug!(
                "Build anchored block off of {}/{} height {}",
                &tip_consensus_hash, &tip_block_hash, tip_height
            );


            let stacks_tip_header_opt = StacksChainState::get_anchored_block_header_info(
                chainstate.db(),
                &parent_consensus_hash,
                &anchored_block_hash,
            )?;

            if stacks_tip_header_opt.is_none() {
                error!("could not find header for known chain tip {:?} != {:?}", parent_consensus_hash, anchored_block_hash);
                return Err(ChainstateError::NoSuchBlockError);
            }

            let mut stacks_tip_header = stacks_tip_header_opt.unwrap();

            let parent_sortition_id = &parent_snapshot.sortition_id;
            // let parent_winning_vtxindex =
            //     match SortitionDB::get_block_winning_vtxindex(burn_db.conn(), parent_sortition_id)
            //         .expect("SortitionDB failure.")
            //     {
            //         Some(x) => x,
            //         None => {
            //             warn!(
            //                 "Failed to find winning vtx index for the parent sortition {}",
            //                 parent_sortition_id
            //             );
            //             return Err(ChainstateError::NoSuchBlockError);
            //         }
            //     };

            let parent_block_snapshot =
                match SortitionDB::get_block_snapshot(burn_db.conn(), parent_sortition_id)
                    .expect("SortitionDB failure.")
                {
                    Some(x) => x,
                    None => {
                        warn!(
                            "Failed to find block snapshot for the parent sortition {}",
                            parent_sortition_id
                        );
                        return Err(ChainstateError::NoSuchBlockError);
                    }
                };

            // // don't mine off of an old burnchain block
            // let burn_chain_tip = SortitionDB::get_canonical_burn_chain_tip(burn_db.conn())
            //     .expect("FATAL: failed to query sortition DB for canonical burn chain tip");

            // if burn_chain_tip.consensus_hash != burn_block.consensus_hash {
            //     debug!("New canonical burn chain tip detected: {} ({}) > {} ({}). Will not try to mine.", burn_chain_tip.consensus_hash, burn_chain_tip.block_height, &burn_block.consensus_hash, &burn_block.block_height);
            //     return None;
            // }

            // debug!("Mining tenure's last consensus hash: {} (height {} hash {}), stacks tip consensus hash: {} (height {} hash {})",
            //            &burn_block.consensus_hash, burn_block.block_height, &burn_block.burn_header_hash,
            //            &stacks_tip.consensus_hash, parent_snapshot.block_height, &parent_snapshot.burn_header_hash);


            let coinbase_nonce = {
                let principal = keychain.origin_address(mainnet).unwrap().into();
                let account = chainstate
                    .with_read_only_clarity_tx(
                        &burn_db.index_conn(),
                        &StacksBlockHeader::make_index_block_hash(
                            &parent_consensus_hash,
                            &anchored_block_hash,
                        ),
                        |conn| StacksChainState::get_account(conn, &principal),
                    )
                    .expect(&format!(
                        "BUG: stacks tip block {}/{} no longer exists after we queried it",
                        &parent_consensus_hash, &anchored_block_hash
                    ));
                account.nonce
            };

            // TODO(psq): needed?
            // let parent_staging_block = match StacksChainState::load_staging_block_info(
            //     &chainstate.db(),
            //     &StacksBlockHeader::make_index_block_hash(
            //         &parent_consensus_hash,
            //         &anchored_block_hash,
            //     ),
            // ) {
            //     Ok(Some(x)) => x,
            //     Ok(None) => {
            //         debug!(
            //             "{:?}: No block stored for {}/{}",
            //             &self.local_peer,
            //             &parent_consensus_hash,
            //             &anchored_block_hash,
            //         );
            //         return Err(ChainstateError::NoSuchBlockError);
            //     }
            //     Err(e) => {
            //         debug!(
            //             "{:?}: Failed to query header info of {}/{}: {:?}",
            //             &self.local_peer,
            //             &parent_consensus_hash,
            //             &anchored_block_hash,
            //             &e
            //         );
            //         return Err(ChainstateError::NoSuchBlockError);
            //     }
            // };

            // now we have
            // stacks_tip_header,
            // parent_consensus_hash,
            // parent_block_snapshot.block_height,
            // parent_block_snapshot.total_burn,
            // parent_winning_vtxindex,
            // coinbase_nonce,

            // need
            // vrf_public_key/vrf_private_key


            // TODO(psq): get seed from api (seed via naked http??? hick!!!)
            // let mut vrf_keychain = Keychain::default(hex_bytes("").unwrap().clone());
            // let vrf_pk: VRFPublicKey = VRFPublicKey::from_hex("62a466c064502a8c90a6f02b80164c89ed20fa732a0c0d5512d385d2b62aa6c3").unwrap();
            // let vrf_sk: VRFPrivateKey = VRFPrivateKey::from_hex(&"f1539befa7e0ed8fdc90f56d086ffc69e6bad75f3b9fa96006185ffd4b48e860".to_string()).unwrap();
            // vrf_keychain.add(&vrf_pk, &vrf_sk);

            // Generates a proof out of the sortition hash provided in the params.
            let vrf_proof = match keychain.generate_proof(
                &vrf_pk,
                parent_snapshot.sortition_hash.as_bytes(),
            ) {
                Some(vrfp) => vrfp,
                None => {
                    return Err(ChainstateError::InvalidProof);
                }
            };

            debug!(
                "Generated VRF Proof: {:#?} over {:#?} with key {:#?}",
                vrf_proof,
                &parent_snapshot.sortition_hash,
                &vrf_pk,
            );

            // Generates a new secret key for signing the trail of microblocks
            // of the upcoming tenure.
            let microblock_secret_key = keychain.rotate_microblock_keypair(parent_snapshot.block_height);
            let mblock_pubkey_hash = Hash160::from_node_public_key(&StacksPublicKey::from_private(&microblock_secret_key));

            let coinbase_tx = inner_generate_coinbase_tx(
                keychain,
                coinbase_nonce,
                mainnet,
                chain_id,
            );

            // find the longest microblock tail we can build off of
            let microblock_info_opt =
                match StacksChainState::load_descendant_staging_microblock_stream_with_poison(
                    chainstate.db(),
                    &StacksBlockHeader::make_index_block_hash(
                        &parent_consensus_hash,
                        &anchored_block_hash,
                    ),
                    0,
                    u16::MAX,
                ) {
                    Ok(x) => {
                        let num_mblocks = x.as_ref().map(|(mblocks, ..)| mblocks.len()).unwrap_or(0);
                        debug!(
                            "Loaded {} microblocks descending from {}/{}",
                            num_mblocks,
                            &parent_consensus_hash,
                            &anchored_block_hash
                        );
                        x
                    }
                    Err(e) => {
                        warn!(
                            "Failed to load descendant microblock stream from {}/{}: {:?}",
                            &parent_consensus_hash,
                            &anchored_block_hash,
                            &e
                        );
                        None
                    }
                };

            if let Some((microblocks, poison_opt)) = microblock_info_opt {
                if let Some(ref tail) = microblocks.last() {
                    debug!(
                        "Confirm microblock stream tailed at {} (seq {})",
                        &tail.block_hash(),
                        tail.header.sequence
                    );
                }

                stacks_tip_header.microblock_tail =
                    microblocks.last().clone().map(|blk| blk.header.clone());

                if let Some(poison_payload) = poison_opt {
                    let poison_microblock_tx = inner_generate_poison_microblock_tx(
                        keychain,
                        coinbase_nonce + 1,
                        poison_payload,
                        mainnet,
                        chain_id,
                    );

                    // submit the poison payload, privately, so we'll mine it when building the
                    // anchored block.
                    if let Err(e) = mempool.submit(
                        chainstate,
                        &parent_consensus_hash,
                        &stacks_tip_header.anchored_header.block_hash(),
                        &poison_microblock_tx,
                    ) {
                        warn!(
                            "Detected but failed to mine poison-microblock transaction: {:?}",
                            &e
                        );
                    }
                }
            }

            let (anchored_block, _, _) = match StacksBlockBuilder::build_anchored_block(
                chainstate,
                &burn_db.index_conn(),
                mempool,
                &stacks_tip_header,
                parent_block_snapshot.total_burn,
                vrf_proof.clone(),
                mblock_pubkey_hash,
                &coinbase_tx,
                HELIUM_BLOCK_LIMIT.clone(),
                txids,
            ) {
                Ok(block) => block,
                Err(e) => {
                    error!("Failure mining anchored block: {}", e);
                    return Err(ChainstateError::FailedToMineBlock);
                }
            };
            let block_height = anchored_block.header.total_work.work;
            info!(
                "Succeeded assembling {} block #{}: {}, with {} txs",
                if parent_block_snapshot.total_burn == 0 {
                    "Genesis"
                } else {
                    "Stacks"
                },
                block_height,
                anchored_block.block_hash(),
                anchored_block.txs.len(),
            );

            // let's figure out the recipient set!
            // TODO(psq): this wrongly assumes the next block 
            // however, we should be seeing a lot more of "does not match expected" error
            // create a separate api for helping adjust commits?
            let recipients = match get_next_recipientsPSQ(
                &parent_snapshot,
                chainstate,
                burn_db,
                burnchain,
                &OnChainRewardSetProvider(),
            ) {
                Ok(x) => x,
                Err(e) => {
                    error!("Failure fetching recipient set: {:?}", e);
                    return Err(ChainstateError::FailedToComputeRecipients);
                }
            };

            // let sunset_burn = burnchain.expected_sunset_burn(parent_snapshot.block_height + 1, burn_fee_cap);
            // let rest_commit = burn_fee_cap - sunset_burn;

            let commit_outs = if target_burn_block < burnchain.pox_constants.sunset_end
                && !burnchain.is_in_prepare_phase(target_burn_block)
            {
                RewardSetInfo::into_commit_outs(recipients, mainnet)
            } else {
                vec![StacksAddress::burn_address(mainnet)]
            };

            // println!("StoreMinerBlock");
            // // TODO(psq): this is where the relayer should be an option?
            // // store the block in case it wins sortition
            // if rpc_channel.is_some() {
            //     // TODO(psq): parent_snapshot.burn_header_hash is not the right burn, needs to be highest known instead if we missed a block
            //     let rpc_status = rpc_channel.unwrap().send(RPCDirective::StoreMinerBlock(tip_consensus_hash, parent_snapshot.burn_header_hash, anchored_block.clone(), microblock_secret_key));
            //     println!("rpc_status {:?}", rpc_status);
            //     if !rpc_status.is_ok() {
            //         return Err(ChainstateError::FailedToStoreBlock);
            //     }            
            // }

            // parent_block_height,
            // parent_tx_offset,
            // key_block_height,
            // key_tx_offset,


            let assembled_block = AssembledAnchorBlock {
                parent_consensus_hash: parent_consensus_hash,
                my_burn_hash: parent_snapshot.burn_header_hash,
                anchored_block,
                attempt: 0,
            };

            Ok((
                assembled_block,
                VRFSeed::from_proof(&vrf_proof), // VRFSeed::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap(),
                commit_outs,
                microblock_secret_key,
            ))
        } else {
            // no block with such a consensus
            error!("No block with this consensus {:?}", parent_consensus_hash);
            return Err(ChainstateError::NoSuchBlockError);
        }
    }


    // return stack's parent's burn header hash,
    //        the anchored block,
    //        the burn header hash of the burnchain tip
    fn relayer_run_tenure(
        config: &Config,
        registered_key: RegisteredKey,
        chain_state: &mut StacksChainState,
        burn_db: &mut SortitionDB,
        burnchain: &Burnchain,
        burn_block: BlockSnapshot,
        keychain: &mut Keychain,
        mem_pool: &mut MemPoolDB,
        burn_fee_cap: u64,
        bitcoin_controller: &mut BitcoinRegtestController,
        last_mined_blocks: &Vec<&AssembledAnchorBlock>,
    ) -> Option<(AssembledAnchorBlock, Secp256k1PrivateKey)> {
        let (
            mut stacks_parent_header,
            parent_consensus_hash,
            parent_block_burn_height,
            parent_block_total_burn,
            parent_winning_vtxindex,
            coinbase_nonce,
        ) = if let Some(stacks_tip) = chain_state.get_stacks_chain_tip(burn_db).unwrap() {
            let stacks_tip_header = match StacksChainState::get_anchored_block_header_info(
                chain_state.db(),
                &stacks_tip.consensus_hash,
                &stacks_tip.anchored_block_hash,
            )
            .unwrap()
            {
                Some(x) => x,
                None => {
                    error!("Could not mine new tenure, since could not find header for known chain tip.");
                    return None;
                }
            };

            // the consensus hash of my Stacks block parent
            let parent_consensus_hash = stacks_tip.consensus_hash.clone();

            // the stacks block I'm mining off of's burn header hash and vtxindex:
            let parent_snapshot = SortitionDB::get_block_snapshot_consensus(
                burn_db.conn(),
                &stacks_tip.consensus_hash,
            )
            .expect("Failed to look up block's parent snapshot")
            .expect("Failed to look up block's parent snapshot");

            let parent_sortition_id = &parent_snapshot.sortition_id;
            let parent_winning_vtxindex =
                match SortitionDB::get_block_winning_vtxindex(burn_db.conn(), parent_sortition_id)
                    .expect("SortitionDB failure.")
                {
                    Some(x) => x,
                    None => {
                        warn!(
                            "Failed to find winning vtx index for the parent sortition {}",
                            parent_sortition_id
                        );
                        return None;
                    }
                };

            let parent_block =
                match SortitionDB::get_block_snapshot(burn_db.conn(), parent_sortition_id)
                    .expect("SortitionDB failure.")
                {
                    Some(x) => x,
                    None => {
                        warn!(
                            "Failed to find block snapshot for the parent sortition {}",
                            parent_sortition_id
                        );
                        return None;
                    }
                };

            // don't mine off of an old burnchain block
            let burn_chain_tip = SortitionDB::get_canonical_burn_chain_tip(burn_db.conn())
                .expect("FATAL: failed to query sortition DB for canonical burn chain tip");

            if burn_chain_tip.consensus_hash != burn_block.consensus_hash {
                debug!("New canonical burn chain tip detected: {} ({}) > {} ({}). Will not try to mine.", burn_chain_tip.consensus_hash, burn_chain_tip.block_height, &burn_block.consensus_hash, &burn_block.block_height);
                return None;
            }

            debug!("Mining tenure's last consensus hash: {} (height {} hash {}), stacks tip consensus hash: {} (height {} hash {})",
                       &burn_block.consensus_hash, burn_block.block_height, &burn_block.burn_header_hash,
                       &stacks_tip.consensus_hash, parent_snapshot.block_height, &parent_snapshot.burn_header_hash);

            let coinbase_nonce = {
                let principal = keychain.origin_address(config.is_mainnet()).unwrap().into();
                let account = chain_state
                    .with_read_only_clarity_tx(
                        &burn_db.index_conn(),
                        &StacksBlockHeader::make_index_block_hash(
                            &stacks_tip.consensus_hash,
                            &stacks_tip.anchored_block_hash,
                        ),
                        |conn| StacksChainState::get_account(conn, &principal),
                    )
                    .expect(&format!(
                        "BUG: stacks tip block {}/{} no longer exists after we queried it",
                        &stacks_tip.consensus_hash, &stacks_tip.anchored_block_hash
                    ));
                account.nonce
            };

            (
                stacks_tip_header,
                parent_consensus_hash,
                parent_block.block_height,
                parent_block.total_burn,
                parent_winning_vtxindex,
                coinbase_nonce,
            )
        } else {
            debug!("No Stacks chain tip known, will return a genesis block");
            let (network, _) = config.burnchain.get_bitcoin_network();
            let burnchain_params =
                BurnchainParameters::from_params(&config.burnchain.chain, &network)
                    .expect("Bitcoin network unsupported");

            let chain_tip = ChainTip::genesis(
                &burnchain_params.first_block_hash,
                burnchain_params.first_block_height.into(),
                burnchain_params.first_block_timestamp.into(),
            );

            (
                chain_tip.metadata,
                FIRST_BURNCHAIN_CONSENSUS_HASH.clone(),
                0,
                0,
                0,
                0,
            )
        };

        // has the tip changed from our previously-mined block for this epoch?
        let attempt = {
            let mut best_attempt = 0;
            debug!(
                "Consider {} in-flight Stacks tip(s)",
                &last_mined_blocks.len()
            );
            for prev_block in last_mined_blocks.iter() {
                debug!(
                    "Consider in-flight Stacks tip {}/{} in {}",
                    &prev_block.parent_consensus_hash,
                    &prev_block.anchored_block.header.parent_block,
                    &prev_block.my_burn_hash
                );
                if prev_block.parent_consensus_hash == parent_consensus_hash
                    && prev_block.my_burn_hash == burn_block.burn_header_hash
                    && prev_block.anchored_block.header.parent_block
                        == stacks_parent_header.anchored_header.block_hash()
                {
                    // the anchored chain tip hasn't changed since we attempted to build a block.
                    // But, have discovered any new microblocks worthy of being mined?
                    if let Ok(Some(stream)) =
                        StacksChainState::load_descendant_staging_microblock_stream(
                            chain_state.db(),
                            &StacksBlockHeader::make_index_block_hash(
                                &prev_block.parent_consensus_hash,
                                &stacks_parent_header.anchored_header.block_hash(),
                            ),
                            0,
                            u16::MAX,
                        )
                    {
                        if (prev_block.anchored_block.header.parent_microblock
                            == BlockHeaderHash([0u8; 32])
                            && stream.len() == 0)
                            || (prev_block.anchored_block.header.parent_microblock
                                != BlockHeaderHash([0u8; 32])
                                && stream.len()
                                    <= (prev_block.anchored_block.header.parent_microblock_sequence
                                        as usize)
                                        + 1)
                        {
                            // the chain tip hasn't changed since we attempted to build a block.  Use what we
                            // already have.
                            debug!("Stacks tip is unchanged since we last tried to mine a block ({}/{} at height {} with {} txs, in {} at burn height {}), and no new microblocks ({} <= {})",
                                   &prev_block.parent_consensus_hash, &prev_block.anchored_block.block_hash(), prev_block.anchored_block.header.total_work.work,
                                   prev_block.anchored_block.txs.len(), prev_block.my_burn_hash, parent_block_burn_height, stream.len(), prev_block.anchored_block.header.parent_microblock_sequence);

                            return None;
                        } else {
                            // there are new microblocks!
                            // TODO: only consider rebuilding our anchored block if we (a) have
                            // time, and (b) the new microblocks are worth more than the new BTC
                            // fee minus the old BTC fee
                            debug!("Stacks tip is unchanged since we last tried to mine a block ({}/{} at height {} with {} txs, in {} at burn height {}), but there are new microblocks ({} > {})",
                                   &prev_block.parent_consensus_hash, &prev_block.anchored_block.block_hash(), prev_block.anchored_block.header.total_work.work,
                                   prev_block.anchored_block.txs.len(), prev_block.my_burn_hash, parent_block_burn_height, stream.len(), prev_block.anchored_block.header.parent_microblock_sequence);

                            best_attempt = cmp::max(best_attempt, prev_block.attempt);
                        }
                    } else {
                        // no microblock stream to confirm, and the stacks tip hasn't changed
                        debug!("Stacks tip is unchanged since we last tried to mine a block ({}/{} at height {} with {} txs, in {} at burn height {}), and no microblocks present",
                               &prev_block.parent_consensus_hash, &prev_block.anchored_block.block_hash(), prev_block.anchored_block.header.total_work.work,
                               prev_block.anchored_block.txs.len(), prev_block.my_burn_hash, parent_block_burn_height);

                        return None;
                    }
                } else {
                    debug!("Stacks tip has changed since we last tried to mine a block in {} at burn height {}; attempt was {} (for {}/{})",
                           prev_block.my_burn_hash, parent_block_burn_height, prev_block.attempt, &prev_block.parent_consensus_hash, &prev_block.anchored_block.header.parent_block);
                    best_attempt = cmp::max(best_attempt, prev_block.attempt);
                }
            }
            best_attempt + 1
        };

        // Generates a proof out of the sortition hash provided in the params.
        let vrf_proof = match keychain.generate_proof(
            &registered_key.vrf_public_key,
            burn_block.sortition_hash.as_bytes(),
        ) {
            Some(vrfp) => vrfp,
            None => {
                // Try to recover a key registered in a former session.
                // registered_key.block_height gives us a pointer to the height of the block
                // holding the key register op, but the VRF was derived using the height of one
                // of the parents blocks.
                let _ = keychain.rotate_vrf_keypair(registered_key.block_height - 1);
                match keychain.generate_proof(
                    &registered_key.vrf_public_key,
                    burn_block.sortition_hash.as_bytes(),
                ) {
                    Some(vrfp) => vrfp,
                    None => {
                        error!(
                            "Failed to generate proof with {:?}",
                            &registered_key.vrf_public_key
                        );
                        return None;
                    }
                }
            }
        };

        debug!(
            "Generated VRF Proof: {} over {} with key {}",
            vrf_proof.to_hex(),
            &burn_block.sortition_hash,
            &registered_key.vrf_public_key.to_hex()
        );

        // Generates a new secret key for signing the trail of microblocks
        // of the upcoming tenure.
        let microblock_secret_key = if attempt > 1 {
            match keychain.get_microblock_key() {
                Some(k) => k,
                None => {
                    error!(
                        "Failed to obtain microblock key for mining attempt";
                        "attempt" => %attempt
                    );
                    return None;
                }
            }
        } else {
            keychain.rotate_microblock_keypair(burn_block.block_height)
        };
        let mblock_pubkey_hash =
            Hash160::from_node_public_key(&StacksPublicKey::from_private(&microblock_secret_key));

        let coinbase_tx = inner_generate_coinbase_tx(
            keychain,
            coinbase_nonce,
            config.is_mainnet(),
            config.burnchain.chain_id,
        );

        // find the longest microblock tail we can build off of
        let microblock_info_opt =
            match StacksChainState::load_descendant_staging_microblock_stream_with_poison(
                chain_state.db(),
                &StacksBlockHeader::make_index_block_hash(
                    &parent_consensus_hash,
                    &stacks_parent_header.anchored_header.block_hash(),
                ),
                0,
                u16::MAX,
            ) {
                Ok(x) => {
                    let num_mblocks = x.as_ref().map(|(mblocks, ..)| mblocks.len()).unwrap_or(0);
                    debug!(
                        "Loaded {} microblocks descending from {}/{}",
                        num_mblocks,
                        &parent_consensus_hash,
                        &stacks_parent_header.anchored_header.block_hash()
                    );
                    x
                }
                Err(e) => {
                    warn!(
                        "Failed to load descendant microblock stream from {}/{}: {:?}",
                        &parent_consensus_hash,
                        &stacks_parent_header.anchored_header.block_hash(),
                        &e
                    );
                    None
                }
            };

        if let Some((microblocks, poison_opt)) = microblock_info_opt {
            if let Some(ref tail) = microblocks.last() {
                debug!(
                    "Confirm microblock stream tailed at {} (seq {})",
                    &tail.block_hash(),
                    tail.header.sequence
                );
            }

            stacks_parent_header.microblock_tail =
                microblocks.last().clone().map(|blk| blk.header.clone());

            if let Some(poison_payload) = poison_opt {
                let poison_microblock_tx = inner_generate_poison_microblock_tx(
                    keychain,
                    coinbase_nonce + 1,
                    poison_payload,
                    config.is_mainnet(),
                    config.burnchain.chain_id,
                );

                // submit the poison payload, privately, so we'll mine it when building the
                // anchored block.
                if let Err(e) = mem_pool.submit(
                    chain_state,
                    &parent_consensus_hash,
                    &stacks_parent_header.anchored_header.block_hash(),
                    &poison_microblock_tx,
                ) {
                    warn!(
                        "Detected but failed to mine poison-microblock transaction: {:?}",
                        &e
                    );
                }
            }
        }

        let (anchored_block, _, _) = match StacksBlockBuilder::build_anchored_block(
            chain_state,
            &burn_db.index_conn(),
            mem_pool,
            &stacks_parent_header,
            parent_block_total_burn,
            vrf_proof.clone(),
            mblock_pubkey_hash,
            &coinbase_tx,
            HELIUM_BLOCK_LIMIT.clone(),
            None,
        ) {
            Ok(block) => block,
            Err(e) => {
                error!("Failure mining anchored block: {}", e);
                return None;
            }
        };
        let block_height = anchored_block.header.total_work.work;
        info!(
            "Succeeded assembling {} block #{}: {}, with {} txs, attempt {}",
            if parent_block_total_burn == 0 {
                "Genesis"
            } else {
                "Stacks"
            },
            block_height,
            anchored_block.block_hash(),
            anchored_block.txs.len(),
            attempt
        );

        // let's figure out the recipient set!
        let recipients = match get_next_recipients(
            &burn_block,
            chain_state,
            burn_db,
            burnchain,
            &OnChainRewardSetProvider(),
        ) {
            Ok(x) => x,
            Err(e) => {
                error!("Failure fetching recipient set: {:?}", e);
                return None;
            }
        };

        let sunset_burn = burnchain.expected_sunset_burn(burn_block.block_height + 1, burn_fee_cap);
        let rest_commit = burn_fee_cap - sunset_burn;

        let commit_outs = if burn_block.block_height + 1 < burnchain.pox_constants.sunset_end
            && !burnchain.is_in_prepare_phase(burn_block.block_height + 1)
        {
            RewardSetInfo::into_commit_outs(recipients, config.is_mainnet())
        } else {
            vec![StacksAddress::burn_address(config.is_mainnet())]
        };

        // let's commit
        let op = inner_generate_block_commit_op(
            keychain.get_burnchain_signer(),
            anchored_block.block_hash(),
            rest_commit,
            &registered_key,
            parent_block_burn_height
                .try_into()
                .expect("Could not convert parent block height into u32"),
            parent_winning_vtxindex,
            VRFSeed::from_proof(&vrf_proof),
            commit_outs,
            sunset_burn,
            burn_block.block_height,
        );
        let mut op_signer = keychain.generate_op_signer();
        debug!(
            "Submit block-commit for block {} off of {}/{} with microblock parent {}",
            &anchored_block.block_hash(),
            &parent_consensus_hash,
            &anchored_block.header.parent_block,
            &anchored_block.header.parent_microblock
        );

        let res = bitcoin_controller.submit_operation(op, &mut op_signer, attempt);
        if res.is_none() {
            warn!("Failed to submit Bitcoin transaction");
            return None;
        }

        Some((
            AssembledAnchorBlock {
                parent_consensus_hash: parent_consensus_hash,
                my_burn_hash: burn_block.burn_header_hash,
                anchored_block,
                attempt,
            },
            microblock_secret_key,
        ))
    }

    /// Process a state coming from the burnchain, by extracting the validated KeyRegisterOp
    /// and inspecting if a sortition was won.
    /// `ibd`: boolean indicating whether or not we are in the initial block download
    pub fn process_burnchain_state(
        &mut self,
        sortdb: &SortitionDB,
        sort_id: &SortitionId,
        ibd: bool,
    ) -> Option<BlockSnapshot> {
        let mut last_sortitioned_block = None;

        let ic = sortdb.index_conn();

        let block_snapshot = SortitionDB::get_block_snapshot(&ic, sort_id)
            .expect("Failed to obtain block snapshot for processed burn block.")
            .expect("Failed to obtain block snapshot for processed burn block.");
        let block_height = block_snapshot.block_height;

        let block_commits =
            SortitionDB::get_block_commits_by_block(&ic, &block_snapshot.sortition_id)
                .expect("Unexpected SortitionDB error fetching block commits");

        update_active_miners_count_gauge(block_commits.len() as i64);

        let (_, network) = self.config.burnchain.get_bitcoin_network();

        for op in block_commits.into_iter() {
            if op.txid == block_snapshot.winning_block_txid {
                info!(
                    "Received burnchain block #{} including block_commit_op (winning) - {} ({})",
                    block_height,
                    op.apparent_sender.to_bitcoin_address(network),
                    &op.block_header_hash
                );
                last_sortitioned_block = Some((block_snapshot.clone(), op.vtxindex));
            } else {
                if self.is_miner {
                    info!(
                        "Received burnchain block #{} including block_commit_op - {} ({})",
                        block_height,
                        op.apparent_sender.to_bitcoin_address(network),
                        &op.block_header_hash
                    );
                }
            }
        }

        let key_registers =
            SortitionDB::get_leader_keys_by_block(&ic, &block_snapshot.sortition_id)
                .expect("Unexpected SortitionDB error fetching key registers");

        let node_address = Keychain::address_from_burnchain_signer(
            &self.burnchain_signer,
            self.config.is_mainnet(),
        );

        for op in key_registers.into_iter() {
            if op.address == node_address {
                if self.is_miner {
                    info!(
                        "Received burnchain block #{} including key_register_op - {}",
                        block_height, op.address
                    );
                }
                if !ibd {
                    // not in initial block download, so we're not just replaying an old key.
                    // Registered key has been mined
                    if let LeaderKeyRegistrationState::Pending = self.leader_key_registration_state
                    {
                        self.leader_key_registration_state =
                            LeaderKeyRegistrationState::Active(RegisteredKey {
                                vrf_public_key: op.public_key,
                                block_height: op.block_height as u64,
                                op_vtxindex: op.vtxindex as u32,
                            });
                    }
                }
            }
        }

        // no-op on UserBurnSupport ops are not supported / produced at this point.
        self.last_burn_block = Some(block_snapshot);

        last_sortitioned_block.map(|x| x.0)
    }
}

impl NeonGenesisNode {
    /// Instantiate and initialize a new node, given a config
    pub fn new(
        config: Config,
        mut event_dispatcher: EventDispatcher,
        burnchain: Burnchain,
        boot_block_exec: Box<dyn FnOnce(&mut ClarityTx) -> ()>,
    ) -> Self {
        let keychain = Keychain::default(config.node.seed.clone());
        let initial_balances = config
            .initial_balances
            .iter()
            .map(|e| (e.address.clone(), e.amount))
            .collect();

        let mut boot_data =
            ChainStateBootData::new(&burnchain, initial_balances, Some(boot_block_exec));

        // do the initial open!
        let (_chain_state, receipts) = match StacksChainState::open_and_exec(
            config.is_mainnet(),
            config.burnchain.chain_id,
            &config.get_chainstate_path(),
            Some(&mut boot_data),
            config.block_limit.clone(),
        ) {
            Ok(res) => res,
            Err(err) => panic!(
                "Error while opening chain state at path {}: {:?}",
                config.get_chainstate_path(),
                err
            ),
        };

        event_dispatcher.process_boot_receipts(receipts);

        Self {
            keychain,
            config,
            event_dispatcher,
            burnchain,
        }
    }

    pub fn into_initialized_leader_node(
        self,
        burnchain_tip: BurnchainTip,
        blocks_processed: BlocksProcessedCounter,
        coord_comms: CoordinatorChannels,
        sync_comms: PoxSyncWatchdogComms,
        attachments_rx: Receiver<HashSet<AttachmentInstance>>,
        atlas_config: AtlasConfig,
    ) -> InitializedNeonNode {
        let config = self.config;
        let mut keychain = self.keychain;
        let event_dispatcher = self.event_dispatcher;

        let leader_key_registration_state = match fs::read_to_string("vrf_key.json") {
            Ok(json) => {
                // let json = fs::read_to_string(vrf_key_file).expect("Unable to read file");
                let registration_key: RegistrationKey = serde_json::from_str(&json).unwrap();
                info!("reusing registration_key {:?}", registration_key);
                keychain.add(&registration_key.vrf_public_key, &registration_key.vrf_secret_key);

                LeaderKeyRegistrationState::Active(RegisteredKey {
                    vrf_public_key: registration_key.vrf_public_key,
                    block_height: registration_key.block_height,
                    op_vtxindex: registration_key.op_vtxindex,
                })
            },
            _ => {
                info!("vrf_key.json not found, will register a new key");
                LeaderKeyRegistrationState::Inactive
            },
        };

        InitializedNeonNode::new(
            config,
            keychain,
            event_dispatcher,
            Some(burnchain_tip),
            true,
            blocks_processed,
            coord_comms,
            sync_comms,
            self.burnchain,
            attachments_rx,
            atlas_config,
            leader_key_registration_state,
        )
    }

    pub fn into_initialized_node(
        self,
        burnchain_tip: BurnchainTip,
        blocks_processed: BlocksProcessedCounter,
        coord_comms: CoordinatorChannels,
        sync_comms: PoxSyncWatchdogComms,
        attachments_rx: Receiver<HashSet<AttachmentInstance>>,
        atlas_config: AtlasConfig,
    ) -> InitializedNeonNode {
        let config = self.config;
        let keychain = self.keychain;
        let event_dispatcher = self.event_dispatcher;

        InitializedNeonNode::new(
            config,
            keychain,
            event_dispatcher,
            Some(burnchain_tip),
            false,
            blocks_processed,
            coord_comms,
            sync_comms,
            self.burnchain,
            attachments_rx,
            atlas_config,
            LeaderKeyRegistrationState::Inactive,
        )
    }
}
