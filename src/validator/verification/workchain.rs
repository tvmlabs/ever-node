/*
* Copyright (C) 2019-2022 TON Labs. All Rights Reserved.
*
* Licensed under the SOFTWARE EVALUATION License (the "License"); you may not use
* this file except in compliance with the License.
*
* Unless required by applicable law or agreed to in writing, software
* distributed under the License is distributed on an "AS IS" BASIS,
* WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
* See the License for the specific TON DEV software governing permissions and
* limitations under the License.
*/

use super::block::Block;
use super::block::BlockCandidateBody;
use super::block::BlockPtr;
use super::block::MultiSignature;
use super::workchain_overlay::WorkchainOverlay;
use super::workchain_overlay::WorkchainOverlayListener;
use super::*;
use super::utils::HangCheck;
use crate::validator::validator_utils::get_adnl_id;
use crate::validator::validator_utils::sigpubkey_to_publickey;
use catchain::BlockPayloadPtr;
use catchain::PublicKeyHash;
use catchain::profiling::ResultStatusCounter;
use catchain::profiling::InstanceCounter;
use catchain::profiling::check_execution_time;
use log::*;
use rand::Rng;
use tokio::time::sleep;
use std::time::SystemTime;
use std::time::Duration;
use spin::mutex::SpinMutex;
use ton_api::ton::ton_node::blockcandidatestatus::BlockCandidateStatus;
use ton_api::ton::ton_node::broadcast::BlockCandidateBroadcast;
use ton_api::IntoBoxed;
use ton_types::Result;
use validator_session::ValidatorWeight;
use catchain::utils::MetricsDumper;
use catchain::utils::add_compute_relative_metric;
use catchain::utils::add_compute_result_metric;
use ever_bls_lib::bls::BLS_PUBLIC_KEY_LEN;

//TODO: cutoff weight configuration
//TODO: neighbours mode configuration

/*
===============================================================================
    Constants
===============================================================================
*/

const BLOCK_SYNC_MIN_PERIOD_MS: u64 = 300; //min time for block sync
const BLOCK_SYNC_MAX_PERIOD_MS: u64 = 400; //max time for block sync
const NEIGHBOURS_SYNC_MIN_PERIOD_MS: u64 = 1500; //min time for sync with neighbour nodes
const NEIGHBOURS_SYNC_MAX_PERIOD_MS: u64 = 2000; //max time for sync with neighbour nodes
const BLOCK_LIFETIME_PERIOD: Duration = Duration::from_secs(600); //block's lifetime

/*
===============================================================================
    Workchain
===============================================================================
*/

pub type WorkchainPtr = Arc<Workchain>;

//todo: hide fields within module
pub struct Workchain {
    runtime: tokio::runtime::Handle,      //runtime handle for spawns
    validator_set_hash: UInt256,          //hash of validators set
    wc_validators: Vec<ValidatorDescr>,   //WC validators
    wc_pub_keys: Vec<[u8; BLS_PUBLIC_KEY_LEN]>, //WC validators pubkeys
    local_adnl_id: PublicKeyHash,         //ADNL ID for this node
    wc_local_idx: i16,                    //local index in WC validator set
    mc_local_idx: i16,                    //local index in MC validator set
    workchain_id: i32,                    //workchain identifier
    self_weak_ref: SpinMutex<Option<Weak<Workchain>>>, //self weak reference
    wc_cutoff_weight: ValidatorWeight,    //cutoff weight for consensus in WC
    _local_key: PrivateKey,               //private key
    local_bls_key: PrivateKey,            //private BLS key
    local_id: PublicKeyHash,              //local ID for this node
    workchain_overlay: SpinMutex<Option<Arc<WorkchainOverlay>>>, //workchain overlay
    mc_overlay: SpinMutex<Option<Arc<WorkchainOverlay>>>, //MC overlay
    blocks: SpinMutex<HashMap<UInt256, BlockPtr>>, //blocks
    listener: VerificationListenerPtr,    //verification listener
    node_debug_id: Arc<String>,           //node debug ID for workchain
    _workchains_instance_counter: InstanceCounter,                   //workchain instances counter
    blocks_instance_counter: Arc<InstanceCounter>,                   //instance counter for blocks
    merge_block_status_counter: metrics_runtime::data::Counter,      //counter for block updates (via merge with other nodes statuses)
    set_block_status_counter: metrics_runtime::data::Counter,        //counter for set block status (by local node)
    process_block_candidate_counter: metrics_runtime::data::Counter, //counter for block candidates processings
    process_block_status_counter: metrics_runtime::data::Counter,    //counter for block statuses processings
    new_block_candidate_counter: metrics_runtime::data::Counter,     //counter for new block candidates
    send_block_status_to_mc_counter: metrics_runtime::data::Counter, //counter of sendings block status to MC
    send_block_status_counter: metrics_runtime::data::Counter,       //counter of sendings block status within workchain
    verify_block_counter: ResultStatusCounter,                       //counter for block verifications
    block_status_received_in_mc_counter: ResultStatusCounter,        //counter for block receivings in MC
    block_status_send_to_mc_latency_histogram: metrics_runtime::data::Histogram, //histogram for block candidate sending to MC
    block_status_received_in_mc_latency_histogram: metrics_runtime::data::Histogram, //histogram for block candidate receiving in MC
    candidate_delivered_to_wc_latency_histogram: metrics_runtime::data::Histogram, //histogram for block candidate receiving in WC
    block_status_merges_count_histogram: metrics_runtime::data::Histogram, //histogram for block candidate merges count (hops count)
}

impl Workchain {
    /*
        Initialization
    */

    fn bls_key_to_string(key: &Option<[u8; BLS_PUBLIC_KEY_LEN]>) -> String {
        match key {
            None => "N/A".to_string(),
            Some(key) => hex::encode(&key),
        }
    }

    pub async fn create(
        engine: EnginePtr,
        runtime: tokio::runtime::Handle,
        workchain_id: i32,
        wc_validators: Vec<ValidatorDescr>,
        mc_validators: Vec<ValidatorDescr>,
        validator_set_hash: UInt256,
        local_key: &PrivateKey,
        local_bls_key: &PrivateKey,
        listener: VerificationListenerPtr,
        metrics_receiver: Arc<metrics_runtime::Receiver>,
        workchains_instance_counter: Arc<InstanceCounter>,
        blocks_instance_counter: Arc<InstanceCounter>,
        wc_overlays_instance_counter: Arc<InstanceCounter>,
        mc_overlays_instance_counter: Arc<InstanceCounter>,
    ) -> Result<Arc<Self>> {
        let local_id = local_key.id();
        let mut wc_local_idx = -1;
        let mut mc_local_idx = -1;
        let mut local_adnl_id = None;

        for (idx, desc) in wc_validators.iter().enumerate() {
            let public_key = sigpubkey_to_publickey(&desc.public_key);

            if public_key.id() == local_id {
                wc_local_idx = idx as i16;
                local_adnl_id = Some(get_adnl_id(&desc));
                break;
            }
        }

        for (idx, desc) in mc_validators.iter().enumerate() {
            let public_key = sigpubkey_to_publickey(&desc.public_key);

            if public_key.id() == local_id {
                mc_local_idx = idx as i16;
                local_adnl_id = Some(get_adnl_id(&desc));
                break;
            }
        }

        assert!(local_adnl_id.is_some());

        let local_adnl_id = local_adnl_id.as_ref().expect("local_adnl_id must exist").clone();
        let node_debug_id = Arc::new(format!("#{}.{}", workchain_id, local_adnl_id));

        let wc_total_weight: ValidatorWeight = wc_validators.iter().map(|desc| desc.weight).sum();
        let wc_cutoff_weight = wc_total_weight * 2 / 3 + 1;

        let mut wc_pub_keys = Vec::new();

        log::info!(target: "verificator", "Creating verification workchain {} (validator_set_hash={}) with {} workchain nodes (total_weight={}, cutoff_weight={}, wc_local_idx={}, mc_local_idx={})",
            node_debug_id,
            validator_set_hash.to_hex_string(),
            wc_validators.len(),
            wc_total_weight,
            wc_cutoff_weight,
            wc_local_idx,
            mc_local_idx);

        for (i, desc) in wc_validators.iter().enumerate() {
            let adnl_id = get_adnl_id(&desc);
            //let adnl_id = desc.adnl_addr.clone().map_or("** no-addr **".to_string(), |x| x.to_hex_string());
            let public_key = sigpubkey_to_publickey(&desc.public_key);

            log::debug!(target: "verificator", "...node {}#{}/{} for workchain {}: public_key={}, public_key_bls={}, adnl_id={}, weight={} ({:.2}%)",
                if local_id == public_key.id() { ">" } else { " " },
                i, wc_validators.len(), node_debug_id,
                &hex::encode(&catchain::serialize_tl_boxed_object!(&public_key.into_public_key_tl().unwrap()).as_ref()),
                Self::bls_key_to_string(&desc.bls_public_key),
                adnl_id,
                desc.weight,
                desc.weight as f64 / wc_total_weight as f64 * 100.0);

            wc_pub_keys.push(match desc.bls_public_key {
                Some(bls_public_key) => bls_public_key.clone().into(),
                None => [0; BLS_PUBLIC_KEY_LEN],
            });
        }

        let mc_total_weight: ValidatorWeight = mc_validators.iter().map(|desc| desc.weight).sum();
        let mc_cutoff_weight = mc_total_weight * 2 / 3 + 1;

        log::debug!(target: "verificator", "Workchain {} (validator_set_hash={}) has {} linked MC nodes (total_weight={}, cutoff_weight={})",
            node_debug_id,
            validator_set_hash.to_hex_string(),
            mc_validators.len(),
            mc_total_weight,
            mc_cutoff_weight);

        for (i, desc) in mc_validators.iter().enumerate() {
            let adnl_id = get_adnl_id(&desc);
            //let adnl_id = desc.adnl_addr.clone().map_or("** no-addr **".to_string(), |x| x.to_hex_string());
            let public_key = sigpubkey_to_publickey(&desc.public_key);

            log::debug!(target: "verificator", "...MC node {}#{}/{} for workchain {}: public_key={}, adnl_id={}, weight={} ({:.2}%)",
                if local_id == public_key.id() { ">" } else { " " },
                i, mc_validators.len(), node_debug_id,
                &hex::encode(&catchain::serialize_tl_boxed_object!(&public_key.into_public_key_tl().unwrap()).as_ref()),
                adnl_id,
                desc.weight,
                desc.weight as f64 / mc_total_weight as f64 * 100.0);
        }

        let workchain = Self {
            workchain_id,
            node_debug_id,
            runtime: runtime.clone(),
            wc_validators,
            validator_set_hash,
            wc_cutoff_weight,
            _local_key: local_key.clone(),
            local_bls_key: local_bls_key.clone(),
            local_adnl_id,
            local_id: local_id.clone(),
            wc_local_idx,
            mc_local_idx,
            wc_pub_keys,
            blocks: SpinMutex::new(HashMap::new()),
            mc_overlay: SpinMutex::new(None),
            workchain_overlay: SpinMutex::new(None),
            listener,
            self_weak_ref: SpinMutex::new(None),
            _workchains_instance_counter: (*workchains_instance_counter).clone(),
            blocks_instance_counter,
            merge_block_status_counter: metrics_receiver.sink().counter(format!("verificator_wc{}_block_status_merges", workchain_id)),
            set_block_status_counter: metrics_receiver.sink().counter(format!("verificator_wc{}_block_status_sets", workchain_id)),
            process_block_candidate_counter: metrics_receiver.sink().counter(format!("verificator_wc{}_block_candidate_processings", workchain_id)),
            process_block_status_counter: metrics_receiver.sink().counter(format!("verificator_wc{}_block_status_processings", workchain_id)),
            new_block_candidate_counter: metrics_receiver.sink().counter(format!("verificator_wc{}_new_block_candidates", workchain_id)),
            send_block_status_to_mc_counter: metrics_receiver.sink().counter(format!("verificator_wc{}_block_status_to_mc_sends", workchain_id)),
            send_block_status_counter: metrics_receiver.sink().counter(format!("verificator_wc{}_block_status_within_wc_sends", workchain_id)),
            block_status_received_in_mc_counter: ResultStatusCounter::new(&metrics_receiver, &format!("verificator_wc{}_block_status_received_in_mc", workchain_id)),
            verify_block_counter: ResultStatusCounter::new(&metrics_receiver, &format!("verificator_wc{}_block_candidate_verifications", workchain_id)),
            candidate_delivered_to_wc_latency_histogram: metrics_receiver.sink().histogram(format!("time:verificator_wc{}_stage1_block_candidate_delivered_in_wc", workchain_id)),
            block_status_send_to_mc_latency_histogram: metrics_receiver.sink().histogram(format!("time:verificator_wc{}_stage2_block_status_send_to_mc_latency", workchain_id)),
            block_status_received_in_mc_latency_histogram: metrics_receiver.sink().histogram(format!("time:verificator_wc{}_stage3_block_status_received_in_mc_latency", workchain_id)),
            block_status_merges_count_histogram: metrics_receiver.sink().histogram(format!("verificator_wc{}_block_status_merges_count", workchain_id)),
        };
        let workchain = Arc::new(workchain);

        //set self weak reference

        *workchain.self_weak_ref.lock() = Some(Arc::downgrade(&workchain));

        //start overlay for interactions with MC

        let mc_overlay_id = { //specific for the workchain_id overlay ID between all MC nodes and all WC nodes
            let (_overlay_short_id, overlay_id) =
                engine.calc_overlay_id(workchain_id, ton_block::SHARD_FULL)?;

            let magic_suffix = [0xff, 0xbe, 0x45, 0x23]; //magic suffix to create unique hash different from public overlay hashes
            let mut overlay_id = overlay_id.to_vec();

            overlay_id.extend_from_slice(&magic_suffix);

            UInt256::calc_file_hash(&overlay_id)
        };

        let mut full_validators = mc_validators.clone();
        full_validators.append(&mut workchain.wc_validators.clone());

        let mc_overlay_listener: Arc<dyn WorkchainOverlayListener> = workchain.clone();
        let mc_overlay = WorkchainOverlay::create(
            workchain.workchain_id,
            format!("MC[{}]{}", workchain.mc_local_idx, *workchain.node_debug_id),
            mc_overlay_id,
            &full_validators,
            mc_validators.len(), //only part of nodes are active
            workchain.local_adnl_id.clone(),
            Arc::downgrade(&mc_overlay_listener),
            &engine,
            runtime.clone(),
            metrics_receiver.clone(),
            mc_overlays_instance_counter,
            format!("verificator_mc{}_overlay", workchain_id),
            true,
        ).await?;
        *workchain.mc_overlay.lock() = Some(mc_overlay);

        if wc_local_idx != -1 {
            //start overlay for private interactions

            let workchain_overlay_listener: Arc<dyn WorkchainOverlayListener> = workchain.clone();
            let workchain_overlay = WorkchainOverlay::create(
                workchain.workchain_id,
                format!("WC[{}]{}", workchain.wc_local_idx, *workchain.node_debug_id),
                workchain.validator_set_hash.clone(),
                &workchain.wc_validators,
                workchain.wc_validators.len(),
                workchain.local_adnl_id.clone(),
                Arc::downgrade(&workchain_overlay_listener),
                &engine,
                runtime.clone(),
                metrics_receiver,
                wc_overlays_instance_counter,
                format!("verificator_wc{}_overlay", workchain_id),
                false,
            ).await?;
            *workchain.workchain_overlay.lock() = Some(workchain_overlay);
        }

        Ok(workchain)
    }

    /*
        Dumper
    */

    pub fn configure_dumper(&self, metrics_dumper: &mut MetricsDumper) {
        log::debug!(target: "verificator", "Creating verification workchain {} metrics dumper", self.node_debug_id);

        let workchain_id = self.workchain_id;

        metrics_dumper.add_derivative_metric(format!("verificator_wc{}_block_candidate_verifications.total", workchain_id));
        metrics_dumper.add_derivative_metric(format!("verificator_wc{}_block_candidate_verifications.success", workchain_id));
        metrics_dumper.add_derivative_metric(format!("verificator_wc{}_block_candidate_verifications.failure", workchain_id));
        metrics_dumper.add_derivative_metric(format!("verificator_wc{}_new_block_candidates", workchain_id));
        metrics_dumper.add_derivative_metric(format!("verificator_mc{}_overlay_in_queries", workchain_id));
        metrics_dumper.add_derivative_metric(format!("verificator_mc{}_overlay_out_queries.total", workchain_id));

        metrics_dumper.add_derivative_metric(format!("verificator_wc{}_block_status_processings", workchain_id));

        add_compute_result_metric(metrics_dumper, &format!("verificator_wc{}_block_candidate_verifications", workchain_id));
        add_compute_result_metric(metrics_dumper, &format!("verificator_mc{}_overlay_out_queries", workchain_id));

        add_compute_relative_metric(
            metrics_dumper,
            &format!("verificator_wc{}_merges_per_block", workchain_id),
            &format!("verificator_wc{}_block_status_merges", workchain_id),
            &format!("verificator_wc{}_new_block_candidates", workchain_id),
            0.0,
        );

        add_compute_relative_metric(
            metrics_dumper,
            &format!("verificator_wc{}_updates_per_mc_send", workchain_id),
            &format!("verificator_wc{}_block_status_processings", workchain_id),
            &format!("verificator_wc{}_block_status_to_mc_sends", workchain_id),
            0.0,
        );

        add_compute_relative_metric(
            metrics_dumper,
            &format!("verificator_wc{}_mc_sends_per_block_candidate", workchain_id),
            &format!("verificator_wc{}_block_status_to_mc_sends", workchain_id),
            &format!("verificator_wc{}_new_block_candidates", workchain_id),
            0.0,
        );

        if workchain_id != -1 {
            add_compute_result_metric(metrics_dumper, &format!("verificator_wc{}_overlay_out_queries", workchain_id));

            metrics_dumper.add_derivative_metric(format!("verificator_wc{}_overlay_in_broadcasts", workchain_id));
            metrics_dumper.add_derivative_metric(format!("verificator_wc{}_overlay_out_broadcasts", workchain_id));
            metrics_dumper.add_derivative_metric(format!("verificator_wc{}_overlay_in_queries", workchain_id));
            metrics_dumper.add_derivative_metric(format!("verificator_wc{}_in_block_candidates", workchain_id));
            metrics_dumper.add_derivative_metric(format!("verificator_wc{}_overlay_out_queries.total", workchain_id));
            metrics_dumper.add_derivative_metric(format!("verificator_wc{}_overlay_send_message_to_neighbours_calls", workchain_id));
        }
    }

    /*
        Common methods
    */

    /// Validator set hash
    pub fn get_validator_set_hash(&self) -> &UInt256 {
        &self.validator_set_hash
    }

    /// Get self weak reference
    fn get_self(&self) -> WorkchainPtr {
        self.self_weak_ref
            .lock()
            .clone()
            .expect("Self ref must be set")
            .upgrade()
            .expect("Self ref must exist")
    }

    /*
        Block management
    */

    /// Block status (delivered, rejected)
    pub fn get_block_status(&self, block: &BlockPtr) -> (bool, bool) {
        let block = block.lock();
        let is_delivered = block.is_delivered(&self.wc_validators, self.wc_cutoff_weight);
        let is_rejected = block.is_rejected();

        (is_delivered, is_rejected)
    }

    /// Should block be sent to MC
    fn should_send_to_mc(&self, block: &BlockPtr) -> bool {
        let (is_delivered, is_rejected) = self.get_block_status(block);

        is_rejected || is_delivered
    }

    /// Get block by its ID
    pub fn get_block_by_id(&self, candidate_id: &UInt256) -> Option<BlockPtr> {
        Self::get_block_by_id_impl(&self.blocks.lock(), candidate_id)
    }

    /// Get block by its ID without lock
    fn get_block_by_id_impl(blocks: &HashMap<UInt256, BlockPtr>, candidate_id: &UInt256) -> Option<BlockPtr> {
        match blocks.get(&candidate_id) {
            Some(block) => Some(block.clone()),
            None => None,
        }
    }

    /// Put new block to map
    fn add_block_impl(
        &self,
        candidate_id: &UInt256,
        block_candidate: Option<Arc<BlockCandidateBody>>,
    ) -> BlockPtr {
        check_execution_time!(1_000);

        let block = {
            let mut blocks = self.blocks.lock();

            match Self::get_block_by_id_impl(&blocks, candidate_id) {
                Some(existing_block) => existing_block,
                None => {
                    trace!(target: "verificator", "Creating new block {:?} for workchain {}", candidate_id, self.node_debug_id);

                    let new_block = Block::create(candidate_id.clone(), block_candidate.clone(), &*self.blocks_instance_counter);

                    blocks.insert(candidate_id.clone(), new_block.clone());

                    drop(blocks); //to release lock

                    self.start_synchronizing_block(&new_block);

                    new_block
                }
            }
        };

        if let Some(block_candidate) = block_candidate {
            let status = block.lock().update_block_candidate(block_candidate.clone());

            trace!(target: "verificator", "Block {:?} status is {} (node={})", candidate_id, status, self.node_debug_id);

            if status
            {
                trace!(target: "verificator", "Block candidate {} is delivered (node={})", candidate_id, self.node_debug_id);

                //measure latency for initial delivery

                let latency = block.lock().get_delivery_latency();
                if let Some(latency) = latency {
                    self.candidate_delivered_to_wc_latency_histogram.record_value(latency.as_millis() as u64);
                }

                //set block status to delivered

                self.set_block_status(&candidate_id, None);

                //initiate verification of the block

                self.verify_block(candidate_id, block_candidate);
            }
        }

        block
    }

    /// Remove block
    fn remove_block(&self, candidate_id: &UInt256) {
        trace!(target: "verificator", "Remove block {:?} for workchain {}", candidate_id, self.node_debug_id);

        self.blocks.lock().remove(candidate_id);
    }

    /// Block update function
    fn synchronize_block(workchain_weak: Weak<Workchain>, block_weak: Weak<SpinMutex<Block>>, neighbours_sync_time: Option<SystemTime>) {
        let workchain = {
            if let Some(workchain) = workchain_weak.upgrade() {
                workchain
            } else {
                return;
            }
        };
        let block = {
            if let Some(block) = block_weak.upgrade() {
                block
            } else {
                return;
            }
        };

        let candidate_id = block.lock().get_id().clone();

        //trace!(target: "verificator", "Synchronize block {:?} for workchain {}", candidate_id, workchain.node_debug_id);
        
        let mut rng = rand::thread_rng();
        let delay = Duration::from_millis(rng.gen_range(
            BLOCK_SYNC_MIN_PERIOD_MS,
            BLOCK_SYNC_MAX_PERIOD_MS + 1,
        ));
        let next_sync_time = SystemTime::now() + delay;
        let neighbours_force_sync = neighbours_sync_time.is_some() && neighbours_sync_time.unwrap().elapsed().is_ok();
        let neighbours_sync_time = {
            if neighbours_sync_time.is_none() || neighbours_force_sync {
                Some(SystemTime::now() + Duration::from_millis(rng.gen_range(NEIGHBOURS_SYNC_MIN_PERIOD_MS, NEIGHBOURS_SYNC_MAX_PERIOD_MS + 1)))
            } else {
                neighbours_sync_time
            }
        };

        let workchain_id = workchain.workchain_id;
        let node_debug_id = workchain.node_debug_id.clone();

        workchain.runtime.spawn(async move {
            if let Ok(timeout) = next_sync_time.duration_since(SystemTime::now()) {
                /*trace!(
                    target: "verificator",
                    "Next block {:?} synchronization for workchain's #{} private overlay is scheduled at {} (in {:.3}s from now; overlay={})",
                    candidate_id,
                    workchain_id,
                    catchain::utils::time_to_string(&next_sync_time),
                    timeout.as_secs_f64(),
                    node_debug_id);*/

                sleep(timeout).await;
            }

            //synchronize block

            Self::synchronize_block(workchain_weak, block_weak, neighbours_sync_time);
        });

        //periodically force push block status to neighbours

        if neighbours_force_sync {
            let (is_delivered, _is_rejected) = workchain.get_block_status(&block);

            if !is_delivered {
                trace!(
                    target: "verificator",
                    "Force block {:?} synchronization for workchain's #{} private overlay (overlay={})",
                    candidate_id,
                    workchain_id,
                    node_debug_id);

                block.lock().toggle_send_ready(true);
            }
        }

        //check if block updates has to be sent to network (updates buffering)

        let ready_to_send = block.lock().toggle_send_ready(false);

        if ready_to_send {
            workchain.send_block_status_impl(&block);
        }

        //remove old blocks

        let block_end_of_life_time = *block.lock().get_first_appearance_time() + BLOCK_LIFETIME_PERIOD;

        if let Ok(_) = block_end_of_life_time.elapsed() {
            workchain.remove_block(&candidate_id);
        }
    }

    /// Start block synchronization
    fn start_synchronizing_block(&self, block: &BlockPtr) {
        Self::synchronize_block(Arc::downgrade(&self.get_self()), Arc::downgrade(block), None);
    }

    /// Put new block to map after delivery
    fn add_delivered_block(&self, block_candidate: Arc<BlockCandidateBody>) -> BlockPtr {
        let candidate_id = Self::get_candidate_id(&block_candidate.candidate());

        //register block

        let block = self.add_block_impl(&candidate_id, Some(block_candidate.clone()));

        block
    }

    /// Merge block status
    fn merge_block_status(
        &self,
        candidate_id: &UInt256,
        deliveries_signature: &MultiSignature,
        approvals_signature: &MultiSignature,
        rejections_signature: &MultiSignature,
        merges_count: u32,
        created_timestamp: i64,
        received_from_workchain: bool,
    ) -> BlockPtr {
        check_execution_time!(5_000);

        self.merge_block_status_counter.increment();

        //get existing block or create it

        let block = self.add_block_impl(candidate_id, None);

        //check if block is MC originated

        if !received_from_workchain {
            block.lock().mark_as_mc_originated();
        }

        //update status

        let status = block.lock().merge_status(deliveries_signature, approvals_signature, rejections_signature, merges_count, created_timestamp
            );
        match status {
            Ok(status) => {
                if !status {
                    //block status is the same
                    return block.clone();
                }
            }
            Err(err) => {
                error!(target: "verificator", "Can't merge block status for block {:?} in workchain {}: {:?}", candidate_id, self.node_debug_id, err);
            }
        }

        //compute latency for MC deliveries

        if !received_from_workchain {
            let was_mc_processed = block.lock().was_mc_processed();
            let (is_delivered, _) = self.get_block_status(&block);

            self.block_status_received_in_mc_counter.total_increment();

            if is_delivered {
                self.block_status_received_in_mc_counter.success();
            } else {
                self.block_status_received_in_mc_counter.failure();
            }

            if !was_mc_processed && is_delivered {
                let latency = block.lock().get_delivery_latency();
                if let Some(latency) = latency {
                    self.block_status_received_in_mc_latency_histogram.record_value(latency.as_millis() as u64);
                }

                self.block_status_merges_count_histogram.record_value(merges_count as u64);

                block.lock().mark_as_mc_processed();
            }
        }

        //block status was updated

        let send_immediately_to_mc = false;

        self.send_block_status(&block, received_from_workchain, send_immediately_to_mc);

        block
    }

    /// Set blocks status (delivered - None, ack - Some(true), nack - Some(false))
    fn set_block_status(&self, candidate_id: &UInt256, status: Option<bool>) {
        check_execution_time!(5_000);

        self.set_block_status_counter.increment();

        //get existing block or create it

        let block = self.add_block_impl(candidate_id, None);

        //update block status

        if self.wc_local_idx != -1 {
            let update_status = block.lock().set_status(&self.local_bls_key, self.wc_local_idx as u16, self.wc_validators.len() as u16, status);

            match update_status {
                Ok(update_status) => {
                    if update_status {
                        //block status was updated
                        let received_from_workchain = true;
                        let send_immediately_to_mc = status.is_some(); //send to MC immediately ACK/NACK without buffering
                        self.send_block_status(&block, received_from_workchain, send_immediately_to_mc);
                    }
                }
                Err(err) => {
                    warn!(target: "verificator", "Can't sign block {} in workchain's node {} private overlay: {:?}", candidate_id, self.node_debug_id, err);
                }
            }
        }
    }

    /*
        Broadcast delivery protection methods
    */

    /// Get candidate ID
    fn get_candidate_id(candidate: &BlockCandidateBroadcast) -> UInt256 {
        Self::get_candidate_id_impl(
            &candidate.id,
            &candidate.collated_data_file_hash,
            &candidate.created_by,
        )
    }

    /// Get candidate ID
    pub fn get_candidate_id_impl(
        id: &BlockIdExt,
        collated_data_file_hash: &UInt256,
        created_by: &UInt256,
    ) -> UInt256 {
        let candidate_id = ::ton_api::ton::validator_session::candidateid::CandidateId {
            src: created_by.clone().into(),
            root_hash: id.root_hash.clone().into(),
            file_hash: id.file_hash.clone().into(),
            collated_data_file_hash: collated_data_file_hash.clone().into(),
        }
        .into_boxed();
        let serialized_candidate_id = catchain::utils::serialize_tl_boxed_object!(&candidate_id);

        catchain::utils::get_hash(&serialized_candidate_id)
    }

    /// Process new block candidate broadcast
    fn process_block_candidate(&self, block_candidate: Arc<BlockCandidateBody>) {
        check_execution_time!(5_000);

        trace!(target: "verificator", "BlockCandidateBroadcast received by verification workchain's node {} private overlay: {:?}", self.node_debug_id, block_candidate.candidate());

        self.process_block_candidate_counter.increment();

        self.add_delivered_block(block_candidate);
    }

    /// New block broadcast has been generated
    pub fn send_new_block_candidate(&self, candidate: BlockCandidateBroadcast) {
        check_execution_time!(5_000);

        let _hang_checker = HangCheck::new(self.runtime.clone(), format!("Workchain::send_new_block_candidate: {:?} for workchain {}", candidate.id, self.node_debug_id), Duration::from_millis(1000));

        self.new_block_candidate_counter.increment();

        //process block candidate

        let block_candidate = Arc::new(BlockCandidateBody::new(candidate));
        let serialized_candidate = block_candidate.serialized_candidate().clone();

        let candidate_id = self.process_block_candidate(block_candidate);

        //send candidate to other workchain validators

        if let Some(workchain_overlay) = self.get_workchain_overlay() {
            trace!(target: "verificator", "Send new block broadcast in workchain {} with candidate_id {:?}", self.node_debug_id, candidate_id);

            workchain_overlay.send_broadcast(
                &self.local_adnl_id,
                &self.local_id,
                serialized_candidate,
            );
        }
    }

    /// Block status update has been received
    pub fn process_block_status(&self, block_status: BlockCandidateStatus, received_from_workchain: bool) -> Result<BlockPtr> {
        check_execution_time!(50_000);

        trace!(target: "verificator", "BlockCandidateStatus received by verification workchain's node {} private overlay: {:?}", self.node_debug_id, block_status);

        self.process_block_status_counter.increment();

        let wc_pub_key_refs: Vec<&[u8; BLS_PUBLIC_KEY_LEN]> = self.wc_pub_keys.iter().map(|x| x).collect();

        let candidate_id: UInt256 = block_status.candidate_id.into();
        let deliveries_signature = MultiSignature::deserialize(1, &candidate_id, &wc_pub_key_refs, &block_status.deliveries_signature);
        let approvals_signature = MultiSignature::deserialize(2, &candidate_id, &wc_pub_key_refs, &block_status.approvals_signature);
        let rejections_signature = MultiSignature::deserialize(3, &candidate_id, &wc_pub_key_refs, &block_status.rejections_signature);

        if let Err(err) = deliveries_signature {
            failure::bail!(
                "Can't parse block candidate status (deliveries signature) {:?}: {:?}",
                block_status,
                err
            );
        }

        if let Err(err) = approvals_signature {
            failure::bail!(
                "Can't parse block candidate status (approvals signature) {:?}: {:?}",
                block_status,
                err
            );
        }

        if let Err(err) = rejections_signature {
            failure::bail!(
                "Can't parse block candidate status (rejections signature) {:?}: {:?}",
                block_status,
                err
            );
        }

        let deliveries_signature = deliveries_signature.unwrap();
        let approvals_signature = approvals_signature.unwrap();
        let rejections_signature = rejections_signature.unwrap();

        Ok(self.merge_block_status(
            &candidate_id,
            &deliveries_signature,
            &approvals_signature,
            &rejections_signature,
            block_status.merges_cnt as u32,
            block_status.created_timestamp,
            received_from_workchain,
        ))
    }

    /// Send block for delivery
    fn send_block_status(&self, block: &BlockPtr, received_from_workchain: bool, send_immediately_to_mc: bool) {
        //serialize block status

        let (serialized_block_status, is_mc_originated, candidate_id, mc_delivered) = {
            //this block is needeed to minimize lock of block
            let mut block = block.lock();
            let candidate_id = block.get_id().clone();

            (block.serialize(), block.is_mc_originated(), candidate_id, block.was_mc_delivered())
        };

        //check if block need to be send to mc

        let should_send_to_mc = self.should_send_to_mc(block) && received_from_workchain && !is_mc_originated && !mc_delivered;

        if send_immediately_to_mc || should_send_to_mc {
            if let Some(mc_overlay) = self.get_mc_overlay() {
                trace!(target: "verificator", "Send block {:?} to MC after update (node={})", candidate_id, self.node_debug_id);

                let mc_overlay = Arc::downgrade(&mc_overlay);
                let serialized_block_status = serialized_block_status.clone();
                let send_block_status_to_mc_counter = self.send_block_status_to_mc_counter.clone();

                let latency = block.lock().get_delivery_latency();
                if let Some(latency) = latency {
                    self.block_status_send_to_mc_latency_histogram.record_value(latency.as_millis() as u64);
                }

                if should_send_to_mc {
                    //prevent double sending of block because of new delivery signatures
                    //do not mark block as delivered for ACK/NACK signals (because they can appear earlier than cutoff weight for delivery BLS)
                    block.lock().mark_as_mc_delivered();
                }

                Self::send_block_status_to_mc(mc_overlay, serialized_block_status, send_block_status_to_mc_counter);
            }
        }

        //mark as ready for send within workchain

        block.lock().toggle_send_ready(true);
    }

    /// Send block for delivery implementation
    fn send_block_status_impl(&self, block: &BlockPtr) {
        self.send_block_status_counter.increment();

        //serialize block status

        let serialized_block_status = {
            //this block is needeed to minimize lock of block
            let mut block = block.lock();
            let candidate_id = block.get_id();

            trace!(target: "verificator", "Send block {:?} to neighbours after update (node={})", candidate_id, self.node_debug_id);

            block.serialize()
        };

        //send block status to neighbours

        self.send_message_to_private_neighbours(serialized_block_status);
    }

    /*
        Verification management
    */

    fn verify_block(&self, candidate_id: &UInt256, block_candidate: Arc<BlockCandidateBody>) {
        trace!(target: "verificator", "Verifying block candidate {} (node={})", candidate_id, self.node_debug_id);

        self.verify_block_counter.total_increment();

        if let Some(verification_listener) = self.listener.upgrade() {
            let candidate_id = candidate_id.clone();
            let workchain = Arc::downgrade(&self.get_self());
            let node_debug_id = self.node_debug_id.clone();
            let runtime = self.runtime.clone();
            let _verification_future = self.runtime.spawn(async move {
                if let Some(workchain) = workchain.upgrade() {
                    check_execution_time!(1_000);
                    let _hang_checker = HangCheck::new(runtime, format!("Workchain::verify_block: {} for workchain {}", candidate_id, node_debug_id), Duration::from_millis(2000));

                    let candidate = super::BlockCandidate {
                        block_id: block_candidate.candidate().id.clone().into(),
                        data: block_candidate.candidate().data.to_vec().into(),
                        collated_file_hash: block_candidate
                            .candidate()
                            .collated_data_file_hash
                            .into(),
                        collated_data: block_candidate.candidate().collated_data.to_vec().into(),
                        created_by: block_candidate.candidate().created_by.into(),
                    };

                    let verification_status = verification_listener.verify(&candidate).await;

                    workchain.set_block_verification_status(&candidate_id, verification_status);
                }
            });
        }
    }

    fn set_block_verification_status(&self, candidate_id: &UInt256, verification_status: bool) {
        trace!(target: "verificator", "Verified block candidate {:?} status is {} (node={})", candidate_id, verification_status, self.node_debug_id);

        if verification_status {
            self.verify_block_counter.success();
        } else {
            self.verify_block_counter.failure();
        }

        self.set_block_status(candidate_id, Some(verification_status));

        if !verification_status {
            error!(target: "verificator", "Malicios block candidate {:?} detected (node={})", candidate_id, self.node_debug_id);
        }
    }

    /*
        Private network (for workchains)
    */

    /// Workchain's private overlay
    fn get_workchain_overlay(&self) -> Option<Arc<WorkchainOverlay>> {
        self.workchain_overlay.lock().clone()
    }

    /// Send message to neighbours in a private workchain overlay
    fn send_message_to_private_neighbours(&self, data: BlockPayloadPtr) {
        if let Some(workchain_overlay) = self.get_workchain_overlay() {
            workchain_overlay.send_message_to_private_neighbours(data);
        }
    }

    /*
        Public network (for interaction with MC)
    */

    /// MC public overlay
    fn get_mc_overlay(&self) -> Option<Arc<WorkchainOverlay>> {
        self.mc_overlay.lock().clone()
    }

    fn send_block_status_to_mc(mc_overlay: Weak<WorkchainOverlay>, data: BlockPayloadPtr, send_block_status_to_mc_counter: metrics_runtime::data::Counter) {
        log::trace!(target: "verificator", "Workchain::send_block_status_to_mc");

        if let Some(mc_overlay) = mc_overlay.upgrade() {
            send_block_status_to_mc_counter.increment();

            mc_overlay.send_all(data);
        }
    }
}

impl WorkchainOverlayListener for Workchain {
    /// Block status has been updated
    fn on_workchain_block_status_updated(
        &self,
        block_status: BlockCandidateStatus,
        received_from_workchain: bool,
    ) -> Result<BlockPtr> {
        let candidate_id: UInt256 = block_status.candidate_id.into();
        let _hang_checker = HangCheck::new(self.runtime.clone(), format!("Workchain::on_workchain_block_status_updated: {} for workchain {}", candidate_id, self.node_debug_id), Duration::from_millis(1000));

        self.process_block_status(block_status, received_from_workchain)
    }

    /// Process new block candidate broadcast
    fn on_workchain_block_candidate(&self, block_candidate: Arc<BlockCandidateBody>) {
        let _hang_checker = HangCheck::new(self.runtime.clone(), format!("Workchain::on_workchain_block_candidate: {:?} for workchain {}", block_candidate.candidate().id, self.node_debug_id), Duration::from_millis(1000));

        self.process_block_candidate(block_candidate);
    }
}

impl Drop for Workchain {
    fn drop(&mut self) {
        log::info!(target: "verificator", "Dropping verification workchain {}", self.node_debug_id);
    }
}