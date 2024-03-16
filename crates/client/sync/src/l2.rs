//! Contains the code required to fetch data from the feeder efficiently.
use std::str::FromStr;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use bitvec::order::Msb0;
use bitvec::view::AsBits;
use bonsai_trie::id::BasicId;
use bonsai_trie::BonsaiStorage;
use itertools::Itertools;
use lazy_static::lazy_static;
use mc_db::bonsai_db::BonsaiDb;
use mc_storage::OverrideHandle;
use mp_block::state_update::StateUpdateWrapper;
use mp_contract::class::{ClassUpdateWrapper, ContractClassData, ContractClassWrapper};
use mp_felt::Felt252Wrapper;
use mp_storage::StarknetStorageSchemaVersion;
use reqwest::Url;
use serde::Deserialize;
use sp_blockchain::HeaderBackend;
use sp_core::{H160, H256};
use sp_runtime::generic::{Block, Header};
use sp_runtime::traits::{BlakeTwo256, Block as BlockT, UniqueSaturatedInto};
use sp_runtime::OpaqueExtrinsic;
use starknet_api::api_core::ClassHash;
use starknet_api::hash::StarkHash;
use starknet_core::types::{BlockId as BlockIdCore, PendingStateUpdate};
use starknet_ff::FieldElement;
use starknet_providers::sequencer::models::state_update::{DeclaredContract, DeployedContract};
use starknet_providers::sequencer::models::{BlockId, StateUpdate};
use starknet_providers::{Provider, SequencerGatewayProvider};
use starknet_types_core::hash::{Pedersen, Poseidon};
use tokio::sync::mpsc::Sender;
use tokio::task::JoinSet;

use crate::commitments::lib::{build_commitment_state_diff, update_state_root};
use crate::CommandSink;

/// Contains the Starknet verified state on L2
#[derive(Debug, Clone, Deserialize)]
pub struct L2StateUpdate {
    pub block_number: u64,
    pub global_root: StarkHash,
    pub block_hash: StarkHash,
}

lazy_static! {
    /// Shared latest L2 state update verified on L2
    pub static ref STARKNET_STATE_UPDATE: Mutex<L2StateUpdate> = Mutex::new(L2StateUpdate {
        block_number: u64::default(),
        global_root: StarkHash::default(),
        block_hash: StarkHash::default(),
    });
}

lazy_static! {
    /// Shared latest block number and hash of chain, using a RwLock to allow for concurrent reads and exclusive writes
    static ref STARKNET_HIGHEST_BLOCK_HASH_AND_NUMBER: RwLock<(FieldElement, u64)> = RwLock::new((FieldElement::default(), 0));
}

lazy_static! {
    /// Shared pending block data, using a RwLock to allow for concurrent reads and exclusive writes
    static ref STARKNET_PENDING_BLOCK: RwLock<Option<mp_block::Block>> = RwLock::new(None);
}

lazy_static! {
    /// Shared pending state update, using RwLock to allow for concurrent reads and exclusive writes
    static ref STARKNET_PENDING_STATE_UPDATE: RwLock<Option<PendingStateUpdate>> = RwLock::new(None);
}

/// The configuration of the worker responsible for fetching new blocks and state updates from the
/// feeder.
#[derive(Clone, Debug)]
pub struct FetchConfig {
    /// The URL of the sequencer gateway.
    pub gateway: Url,
    /// The URL of the feeder gateway.
    pub feeder_gateway: Url,
    /// The ID of the chain served by the sequencer gateway.
    pub chain_id: starknet_ff::FieldElement,
    /// The number of tasks spawned to fetch blocks and state updates.
    pub workers: u32,
    /// Whether to play a sound when a new block is fetched.
    pub sound: bool,
    /// The L1 contract core address
    pub l1_core_address: H160,
}

/// The configuration of the senders responsible for sending blocks and state
/// updates from the feeder.
pub struct SenderConfig {
    /// Sender for dispatching fetched blocks.
    pub block_sender: Sender<mp_block::Block>,
    /// Sender for dispatching fetched state updates.
    pub state_update_sender: Sender<StateUpdateWrapper>,
    /// Sender for dispatching fetched class hashes.
    pub class_sender: Sender<ClassUpdateWrapper>,
    /// The command sink used to notify the consensus engine that a new block
    /// should be created.
    pub command_sink: CommandSink,
    // Storage overrides for accessing stored classes
    pub overrides: Arc<OverrideHandle<Block<Header<u32, BlakeTwo256>, OpaqueExtrinsic>>>,
}

/// Spawns workers to fetch blocks and state updates from the feeder.
pub async fn sync<B, C>(
    mut sender_config: SenderConfig,
    fetch_config: FetchConfig,
    first_block: u64,
    bonsai_contract: &Arc<Mutex<BonsaiStorage<BasicId, BonsaiDb<B>, Pedersen>>>,
    bonsai_class: &Arc<Mutex<BonsaiStorage<BasicId, BonsaiDb<B>, Poseidon>>>,
    client: Arc<C>,
) where
    B: BlockT,
    C: HeaderBackend<B>,
{
    let SenderConfig { block_sender, state_update_sender, class_sender, command_sink, overrides } = &mut sender_config;
    let provider = SequencerGatewayProvider::new(
        fetch_config.gateway.clone(),
        fetch_config.feeder_gateway.clone(),
        fetch_config.chain_id,
    );
    let mut current_block_number = first_block;
    let mut last_block_hash = None;
    let mut got_block = false;
    let mut got_state_update = false;
    let mut last_update_highest_block = tokio::time::Instant::now() - Duration::from_secs(20);

    // TODO: move this somewhere else
    if current_block_number == 1 {
        let _ = fetch_genesis_state_update(
            &provider,
            Arc::clone(overrides),
            Arc::clone(bonsai_contract),
            Arc::clone(bonsai_class),
        )
        .await;
    }

    loop {
        if last_update_highest_block.elapsed() > Duration::from_secs(1) {
            last_update_highest_block = tokio::time::Instant::now();
            if let Err(e) = update_starknet_data(&provider, client.as_ref()).await {
                eprintln!("Failed to update highest block hash and number: {}", e);
            }
        }
        let (block, state_update) = match (got_block, got_state_update) {
            (false, false) => {
                let block = fetch_block(&provider, block_sender, current_block_number);
                let state_update = fetch_state_and_class_update(
                    &provider,
                    current_block_number,
                    Arc::clone(overrides),
                    Arc::clone(bonsai_contract),
                    Arc::clone(bonsai_class),
                    state_update_sender,
                    class_sender,
                    client.as_ref(),
                );
                tokio::join!(block, state_update)
            }
            (false, true) => (fetch_block(&provider, block_sender, current_block_number).await, Ok(())),
            (true, false) => (
                Ok(()),
                fetch_state_and_class_update(
                    &provider,
                    current_block_number,
                    Arc::clone(overrides),
                    Arc::clone(bonsai_contract),
                    Arc::clone(bonsai_class),
                    state_update_sender,
                    class_sender,
                    client.as_ref(),
                )
                .await,
            ),
            (true, true) => unreachable!(),
        };

        got_block = got_block || block.is_ok();
        got_state_update = got_state_update || state_update.is_ok();

        match (block, state_update) {
            (Ok(()), Ok(())) => match create_block(command_sink, &mut last_block_hash).await {
                Ok(()) => {
                    current_block_number += 1;
                    got_block = false;
                    got_state_update = false;
                }
                Err(e) => {
                    eprintln!("Failed to create block: {}", e);
                    return;
                }
            },
            (Err(a), Ok(())) => {
                eprintln!("Failed to fetch block {}: {}", current_block_number, a);
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
            (_, Err(b)) => {
                eprintln!("Failed to fetch state update {}: {}", current_block_number, b);
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    }
}

async fn fetch_block(
    client: &SequencerGatewayProvider,
    block_sender: &Sender<mp_block::Block>,
    block_number: u64,
) -> Result<(), String> {
    let block =
        client.get_block(BlockId::Number(block_number)).await.map_err(|e| format!("failed to get block: {e}"))?;

    let block_conv = crate::convert::block(block).await;
    block_sender.send(block_conv).await.map_err(|e| format!("failed to dispatch block: {e}"))?;

    Ok(())
}

pub async fn fetch_genesis_block(config: FetchConfig) -> Result<mp_block::Block, String> {
    let client = SequencerGatewayProvider::new(config.gateway.clone(), config.feeder_gateway.clone(), config.chain_id);
    let block = client.get_block(BlockId::Number(0)).await.map_err(|e| format!("failed to get block: {e}"))?;

    Ok(crate::convert::block(block).await)
}

#[allow(clippy::too_many_arguments)]
async fn fetch_state_and_class_update<B, C>(
    provider: &SequencerGatewayProvider,
    block_number: u64,
    overrides: Arc<OverrideHandle<Block<Header<u32, BlakeTwo256>, OpaqueExtrinsic>>>,
    bonsai_contract: Arc<Mutex<BonsaiStorage<BasicId, BonsaiDb<B>, Pedersen>>>,
    bonsai_class: Arc<Mutex<BonsaiStorage<BasicId, BonsaiDb<B>, Poseidon>>>,
    state_update_sender: &Sender<StateUpdateWrapper>,
    class_sender: &Sender<ClassUpdateWrapper>,
    client: &C,
) -> Result<(), String>
where
    B: BlockT,
    C: HeaderBackend<B>,
{
    let state_update =
        fetch_state_update(provider, block_number, overrides.clone(), bonsai_contract, bonsai_class, client).await?;
    let class_update = fetch_class_update(provider, &state_update, overrides, block_number, client).await?;

    // Now send state_update, which moves it. This will be received
    // by QueryBlockConsensusDataProvider in deoxys/crates/node/src/service.rs
    state_update_sender
        .send(StateUpdateWrapper::from(state_update))
        .await
        .map_err(|e| format!("failed to dispatch state update: {e}"))?;

    // do the same to class update
    class_sender
        .send(ClassUpdateWrapper(class_update))
        .await
        .map_err(|e| format!("failed to dispatch class update: {e}"))?;

    Ok(())
}

/// retrieves state update from Starknet sequencer
async fn fetch_state_update<B, C>(
    provider: &SequencerGatewayProvider,
    block_number: u64,
    overrides: Arc<OverrideHandle<Block<Header<u32, BlakeTwo256>, OpaqueExtrinsic>>>,
    bonsai_contract: Arc<Mutex<BonsaiStorage<BasicId, BonsaiDb<B>, Pedersen>>>,
    bonsai_class: Arc<Mutex<BonsaiStorage<BasicId, BonsaiDb<B>, Poseidon>>>,
    client: &C,
) -> Result<StateUpdate, String>
where
    B: BlockT,
    C: HeaderBackend<B>,
{
    let state_update = provider
        .get_state_update(BlockId::Number(block_number))
        .await
        .map_err(|e| format!("failed to get state update: {e}"))?;

    let block_hash = block_hash_substrate(client, block_number - 1);
    verify_l2(block_number, &state_update, overrides, bonsai_contract, bonsai_class, block_hash)?;

    Ok(state_update)
}

pub async fn fetch_genesis_state_update<B: BlockT>(
    provider: &SequencerGatewayProvider,
    overrides: Arc<OverrideHandle<Block<Header<u32, BlakeTwo256>, OpaqueExtrinsic>>>,
    bonsai_contract: Arc<Mutex<BonsaiStorage<BasicId, BonsaiDb<B>, Pedersen>>>,
    bonsai_class: Arc<Mutex<BonsaiStorage<BasicId, BonsaiDb<B>, Poseidon>>>,
) -> Result<StateUpdate, String> {
    let state_update =
        provider.get_state_update(BlockId::Number(0)).await.map_err(|e| format!("failed to get state update: {e}"))?;

    verify_l2(0, &state_update, overrides, bonsai_contract, bonsai_class, None)?;

    Ok(state_update)
}

/// retrieves class updates from Starknet sequencer
async fn fetch_class_update<B, C>(
    provider: &SequencerGatewayProvider,
    state_update: &StateUpdate,
    overrides: Arc<OverrideHandle<Block<Header<u32, BlakeTwo256>, OpaqueExtrinsic>>>,
    block_number: u64,
    client: &C,
) -> Result<Vec<ContractClassData>, String>
where
    B: BlockT,
    C: HeaderBackend<B>,
{
    // defaults to downloading ALL classes if a substrate block hash could not be determined
    let missing_classes = match block_hash_substrate(client, block_number) {
        Some(block_hash_substrate) => fetch_missing_classes(state_update, overrides, block_hash_substrate),
        None => aggregate_classes(state_update),
    };

    let arc_provider = Arc::new(provider.clone());
    let mut task_set = missing_classes.into_iter().fold(JoinSet::new(), |mut set, class_hash| {
        set.spawn(download_class(*class_hash, block_hash_madara(state_update), Arc::clone(&arc_provider)));
        set
    });

    // WARNING: all class downloads will abort if even a single class fails to download.
    let mut classes = vec![];
    while let Some(res) = task_set.join_next().await {
        match res {
            Ok(result) => match result {
                Ok(contract) => classes.push(contract),
                Err(e) => {
                    task_set.abort_all();
                    return Err(e.to_string());
                }
            },
            Err(e) => {
                task_set.abort_all();
                return Err(e.to_string());
            }
        }
    }

    Ok(classes)
}

/// Retrieves Madara block hash from state update
fn block_hash_madara(state_update: &StateUpdate) -> FieldElement {
    state_update.block_hash.unwrap()
}

/// Retrieves Substrate block hash from rpc client
fn block_hash_substrate<B, C>(client: &C, block_number: u64) -> Option<H256>
where
    B: BlockT,
    C: HeaderBackend<B>,
{
    client
        .hash(UniqueSaturatedInto::unique_saturated_into(block_number))
        .unwrap()
        .map(|hash| H256::from_slice(hash.as_bits::<Msb0>().to_bitvec().as_raw_slice()))
}

/// Downloads a class definition from the Starknet sequencer. Note that because
/// of the current type hell this needs to be converted into a blockifier equivalent
async fn download_class(
    class_hash: FieldElement,
    block_hash: FieldElement,
    provider: Arc<SequencerGatewayProvider>,
) -> anyhow::Result<ContractClassData> {
    // log::info!("💾 Downloading class {class_hash:#x}");
    let core_class = provider.get_class(BlockIdCore::Hash(block_hash), class_hash).await?;

    // Core classes have to be converted into Blockifier classes to gain support
    // for Substrate [`Encode`] and [`Decode`] traits
    Ok(ContractClassData {
        // TODO: find a less roundabout way of converting from a Felt252Wrapper
        hash: ClassHash(Felt252Wrapper::from(class_hash).into()),
        contract_class: ContractClassWrapper::try_from(core_class)?,
    })
}

/// Filters out class declarations in the Starknet sequencer state update
/// and retains only those which are not stored in the local Substrate db.
fn fetch_missing_classes(
    state_update: &StateUpdate,
    overrides: Arc<OverrideHandle<Block<Header<u32, BlakeTwo256>, OpaqueExtrinsic>>>,
    block_hash_substrate: H256,
) -> Vec<&FieldElement> {
    aggregate_classes(state_update)
        .into_iter()
        .filter(|class_hash| {
            is_missing_class(Arc::clone(&overrides), block_hash_substrate, Felt252Wrapper::from(**class_hash))
        })
        .collect()
}

/// Retrieves all class hashes from state update. This includes newly deployed
/// contract class hashes, Sierra class hashes and Cairo class hashes
fn aggregate_classes(state_update: &StateUpdate) -> Vec<&FieldElement> {
    std::iter::empty()
        .chain(
            state_update
                .state_diff
                .deployed_contracts
                .iter()
                .map(|DeployedContract { address: _, class_hash }| class_hash),
        )
        .chain(
            state_update
                .state_diff
                .declared_classes
                .iter()
                .map(|DeclaredContract { class_hash, compiled_class_hash: _ }| class_hash),
        )
        .unique()
        .collect()
}

/// Check if a class is stored in the local Substrate db.
///
/// Since a change in class definition will result in a change in class hash,
/// this means we only need to check for class hashes in the db.
fn is_missing_class(
    overrides: Arc<OverrideHandle<Block<Header<u32, BlakeTwo256>, OpaqueExtrinsic>>>,
    block_hash_substrate: H256,
    class_hash: Felt252Wrapper,
) -> bool {
    overrides
        .for_schema_version(&StarknetStorageSchemaVersion::Undefined)
        .contract_class_by_class_hash(block_hash_substrate, ClassHash::from(class_hash))
        .is_none()
}

/// Notifies the consensus engine that a new block should be created.
async fn create_block(cmds: &mut CommandSink, parent_hash: &mut Option<H256>) -> Result<(), String> {
    let (sender, receiver) = futures::channel::oneshot::channel();

    cmds.try_send(sc_consensus_manual_seal::rpc::EngineCommand::SealNewBlock {
        create_empty: true,
        finalize: true,
        parent_hash: None,
        sender: Some(sender),
    })
    .unwrap();

    let create_block_info = receiver
        .await
        .map_err(|err| format!("failed to seal block: {err}"))?
        .map_err(|err| format!("failed to seal block: {err}"))?;

    *parent_hash = Some(create_block_info.hash);
    Ok(())
}

/// Update the L2 state with the latest data
pub fn update_l2(state_update: L2StateUpdate) {
    {
        let mut last_state_update =
            STARKNET_STATE_UPDATE.lock().expect("Failed to acquire lock on STARKNET_STATE_UPDATE");
        *last_state_update = state_update.clone();
    }
}

/// Verify and update the L2 state according to the latest state update
pub fn verify_l2<B: BlockT>(
    block_number: u64,
    state_update: &StateUpdate,
    overrides: Arc<OverrideHandle<Block<Header<u32, BlakeTwo256>, OpaqueExtrinsic>>>,
    bonsai_contract: Arc<Mutex<BonsaiStorage<BasicId, BonsaiDb<B>, Pedersen>>>,
    bonsai_class: Arc<Mutex<BonsaiStorage<BasicId, BonsaiDb<B>, Poseidon>>>,
    substrate_block_hash: Option<H256>,
) -> Result<(), String> {
    let state_update_wrapper = StateUpdateWrapper::from(state_update);

    let csd = build_commitment_state_diff(state_update_wrapper.clone());
    let state_root =
        update_state_root(csd, overrides, bonsai_contract, bonsai_class, block_number, substrate_block_hash);
    let block_hash = state_update.block_hash.expect("Block hash not found in state update");

    update_l2(L2StateUpdate {
        block_number,
        global_root: state_root.into(),
        block_hash: Felt252Wrapper::from(block_hash).into(),
    });

    Ok(())
}

async fn update_starknet_data<B, C>(provider: &SequencerGatewayProvider, client: &C) -> Result<(), String>
where
    B: BlockT,
    C: HeaderBackend<B>,
{
    let block = provider.get_block(BlockId::Pending).await.map_err(|e| format!("Failed to get pending block: {e}"))?;

    let hash_best = client.info().best_hash;
    let hash_current = block.parent_block_hash;
    // Well howdy, seems like we can't convert a B::Hash to a FieldElement pa'tner,
    // fancy this instead? 🤠🔫
    let tmp = <B as BlockT>::Hash::from_str(&hash_current.to_string()).unwrap_or(Default::default());
    let number = block.block_number.ok_or("block number not found")? - 1;

    // all blocks have been synchronized, can store pending data
    if hash_best == tmp {
        let state_update = provider
            .get_state_update(BlockId::Pending)
            .await
            .map_err(|e| format!("Failed to get pending state update: {e}"))?;

        // Speaking about type conversion hell: 🔥
        *STARKNET_PENDING_BLOCK.write().expect("Failed to acquire write lock on STARKNET_PENDING_BLOCK") =
            Some(crate::convert::block(block).await);

        // This type conversion is evil and should not be necessary
        *STARKNET_PENDING_STATE_UPDATE.write().expect("Failed to aquire write lock on STARKNET_PENDING_STATE_UPDATE") =
            Some(crate::convert::state_update(state_update));
    }

    *STARKNET_HIGHEST_BLOCK_HASH_AND_NUMBER
        .write()
        .expect("Failed to acquire write lock on STARKNET_HIGHEST_BLOCK_HASH_AND_NUMBER") = (hash_current, number);

    Ok(())
}

pub fn get_highest_block_hash_and_number() -> (FieldElement, u64) {
    *STARKNET_HIGHEST_BLOCK_HASH_AND_NUMBER
        .read()
        .expect("Failed to acquire read lock on STARKNET_HIGHEST_BLOCK_HASH_AND_NUMBER")
}

pub fn get_pending_block() -> Option<mp_block::Block> {
    STARKNET_PENDING_BLOCK.read().expect("Failed to acquire read lock on STARKNET_PENDING_BLOCK").clone()
}

pub fn get_pending_state_update() -> Option<PendingStateUpdate> {
    STARKNET_PENDING_STATE_UPDATE.read().expect("Failed to acquire read lock on STARKNET_PENDING_BLOCK").clone()
}
