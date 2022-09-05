use crate::{
    engine_traits::EngineOperations,
    validator::validator_utils::get_adnl_id,
    shard_state::ShardStateStuff,
    ext_messages::{create_ext_message, is_finally_rejected, is_finally_accepted},
    validator::validator_utils::validatordescr_to_catchain_node,
    block::BlockStuff,
    network::remp::RempReceiptsSubscriber,
    types::shard_blocks_observer::ShardBlocksObserver,
    full_node::telemetry::ReceiptTelemetry,
};
use ton_types::{Result, fail, error, UInt256, AccountId};
use ton_block::{
    Message, ShardIdent, FutureSplitMerge, ShardAccount, BlockIdExt, ValidatorDescr, MASTERCHAIN_ID,
    HashmapAugType
};
use ton_executor::{
    BlockchainConfig, OrdinaryTransactionExecutor, TransactionExecutor, ExecuteParams,
};
use adnl::common::add_unbound_object_to_map_with_update;
use ever_crypto::{KeyId, Ed25519KeyOption};
use std::{
    collections::HashSet, 
    sync::{Arc, atomic::{AtomicU64, Ordering}},
    time::Instant,
    convert::TryInto,
};
use ton_api::{
    IntoBoxed,
    ton::ton_node::{rempmessage::RempMessage, RempMessageLevel, RempMessageStatus, RempReceipt, RempSignedReceipt}
};

const HANGED_MESSAGE_TIMEOUT_MS: u64 = 20_000;
const TIME_BEFORE_DIE_MS: u64 = 100_000;

#[derive(Default)]
pub struct RempClient {
    messages: Arc<lockfree::map::Map<UInt256, RempMessageHistoryInternal>>,
    engine: tokio::sync::OnceCell<Arc<dyn EngineOperations>>,
    local_key_id: UInt256,
    hanged_message_timeout_ms: u64,
    time_before_die_ms: u64,
    skip_run_local: bool,
}

#[derive(Clone)]
pub struct RempMessageHistory {
    pub last_update: u64,
    pub time_to_die: u64,
    pub statuses: Vec<(Vec<u8>, RempReceipt)>,
}

impl From<&RempMessageHistoryInternal> for RempMessageHistory {
    fn from(status: &RempMessageHistoryInternal) -> Self {
        let mut statuses = vec!();
        for status in status.statuses.iter() {
            statuses.push((status.key().clone(), status.val().clone()));
        }
        statuses.sort_by(|a, b| a.1.timestamp().partial_cmp(&b.1.timestamp()).unwrap());
        RempMessageHistory {
            last_update: status.last_update.load(Ordering::Relaxed),
            time_to_die: status.time_to_die.load(Ordering::Relaxed),
            statuses,
        }
    }
}
pub struct RempMessageHistoryInternal {
    pub last_update: AtomicU64,
    pub time_to_die: AtomicU64,
    pub mc_cc_to_die: u32,
    pub statuses: lockfree::map::Map<Vec<u8>, RempReceipt>,
}

#[async_trait::async_trait]
impl RempReceiptsSubscriber for RempClient {
    async fn new_remp_receipt(&self, receipt: RempSignedReceipt, source: &Arc<KeyId>) -> Result<()> {
        let sign_bytes = format!("{}...", hex::encode(&receipt.signature().0[0..6]));
        log::info!("Processing REMP receipt {} from {}", sign_bytes, source);
        self.process_remp_receipt(receipt, source).await
            .map_err(|e| {
                log::error!("Error while processing REMP receipt {} from {}: {}", sign_bytes, source, e);
                e
            })?;
        log::info!("Processed REMP receipt {}  from {}", sign_bytes, source);
        Ok(())
    }
}

impl RempClient {

    pub fn new(local_key_id: UInt256) -> Self {
        Self::with_params(HANGED_MESSAGE_TIMEOUT_MS, TIME_BEFORE_DIE_MS, false, local_key_id)
    }

    pub fn with_params(
        hanged_message_timeout_ms: u64,
        time_before_die_ms: u64, 
        skip_run_local: bool,
        local_key_id: UInt256,
    ) -> Self {
        RempClient {
            hanged_message_timeout_ms,
            time_before_die_ms,
            skip_run_local,
            local_key_id,
            ..Default::default()
        }
    }

    pub async fn start(self: Arc<Self>, engine: Arc<dyn EngineOperations>) -> Result<()> {

        log::trace!("start");

        // set engine
        self.engine.set(engine).map_err(|_| error!("Attempt to set engine twice"))?;

        // resolve current validators
        let (last_mc_block_id, validators) = self.resolve_validators().await?;
        
        // TODO start read messages from kafka worker

        // start mc blocks monitoring
        tokio::spawn(async move {
            if let Err(e) = self.mc_blocks_monitor(last_mc_block_id, validators).await {
                log::error!("FATAL error while mc blocks monitoring: {}", e)
            }
        });

        log::trace!("started");

        Ok(())
    }

    pub fn process_remp_message(
        self: Arc<Self>,
        raw_message: Vec<u8>,
        id: UInt256,
    ) {
        tokio::spawn(async move {
            // process message
            match self.clone().process_remp_message_impl(raw_message, id).await {
                Ok(_) => log::info!("Processed REMP message {:x}", id),
                Err(e) => {
                    log::error!("Error while processing REMP message {:x}: {}", id, e);
                    #[cfg(feature = "telemetry")] {
                        if let Some(engine) = self.engine.get() {
                            engine.remp_client_telemetry().set_message_rejected(&id);
                        }
                    }
                    let status = RempMessageStatus::TonNode_RempRejected (
                        ton_api::ton::ton_node::rempmessagestatus::RempRejected {
                            level: RempMessageLevel::TonNode_RempFullnode,
                            block_id: BlockIdExt::default(),
                            error: e.to_string(),
                        }
                    );
                    if let Err(e) = self.new_self_processing_status(&id, status, false) {
                        log::error!("new_self_processing_status: {}", e);
                    }
                }
            }
        });
    }

    pub fn process_new_block(self: Arc<Self>, block: BlockStuff) {
        tokio::spawn(async move {
            match self.process_block(&block, true, None) {
                Ok(_) => log::trace!("Processed new shard block {}", block.id()),
                Err(e) => {
                    log::error!("Error while processing new shard block {}: {}", block.id(), e);
                }
            }
        });
    }


    async fn mc_blocks_monitor(
        &self,
        last_mc_block_id: BlockIdExt,
        mut validators: HashSet<ValidatorDescr>,
    ) -> Result<()> {
        let engine = self.engine.get().ok_or_else(|| error!("engine was not set"))?;
        let mut prev_handle = engine.load_block_handle(&last_mc_block_id)?
            .ok_or_else(|| error!("Can't load handle for last mc block {}", last_mc_block_id))?;
        let mut shard_blocks_observer = ShardBlocksObserver::new(
            &prev_handle,
            engine.clone(),
            |block, mc_id| { self.process_block(block, true, Some(mc_id)) }
        ).await?;
        
        loop {
            log::trace!("waiting next mc block (prev {})", prev_handle.id());
            let (handle, block) = loop {
                match engine.wait_next_applied_mc_block(&prev_handle, Some(10_000)).await {
                    Ok(b) => break b,
                    Err(e) => {
                        log::warn!("Wait next applied mc block after {}: {}", prev_handle.id(), e);
                        if engine.check_stop() {
                            log::info!("mc_blocks_monitor: finished because engine.check_stop returns true");
                            return Ok(())
                        }
                    }
                }
            };
            log::trace!("next mc block is {}", handle.id());
            prev_handle = handle;

            if let Err(e) = self.process_new_master_block(&block, &mut shard_blocks_observer).await {
                log::error!("Error while check master block for applied messages: {}", e);
            }

            if let Err(e) = self.resolve_validators_if_need(&block, &mut validators).await {
                log::error!("Error while check master block for applied messages: {}", e);
            }
        }
    }

    async fn process_new_master_block<F: Fn(&BlockStuff, &BlockIdExt) -> Result<()>>(
        &self,
        block: &BlockStuff,
        shard_blocks_observer: &mut ShardBlocksObserver<F>
    ) -> Result<()> {

        log::trace!("process_new_master_block {}", block.id());

        let engine = self.engine.get().ok_or_else(|| error!("engine was not set"))?;
        
        let (process_mc, _processed_wc) = engine.processed_workchain().await?;
        
        if process_mc {
            self.process_block(block, true, Some(block.id()))?;
        }

        shard_blocks_observer.process_next_mc_block(block).await?;

        // process hanged messages
        let mc_cc_seqno = block.block().read_info()?.gen_catchain_seqno();
        let now = engine.now_ms();
        let mut processing = 0;
        let mut hanged = 0;
        for msg in self.messages.iter() {
            let time_to_die = msg.val().time_to_die.load(Ordering::Relaxed);
            if time_to_die != 0 {
                if time_to_die < now {
                    self.messages.remove(msg.key());
                    log::debug!("External message {:x} died", msg.key());
                }
            } else {
                let last_update = msg.val().last_update.load(Ordering::Relaxed);
                if mc_cc_seqno >= msg.val().mc_cc_to_die {
                    log::warn!(
                        "External message {:x} will be deleted because mc catchain {} arrives",
                        msg.key(), mc_cc_seqno
                    );
                    self.messages.remove(msg.key());

                    #[cfg(feature = "telemetry")]
                    engine.remp_client_telemetry().set_message_expired(msg.key());
                } else if last_update + self.hanged_message_timeout_ms < now {
                    log::warn!("External message {:x} was not updated more than {}ms",
                        msg.key(), self.hanged_message_timeout_ms
                    );
                    hanged += 1;
                } else {
                    processing += 1;
                }
            }
        }

        #[cfg(feature = "telemetry")]
        engine.remp_client_telemetry().set_current(processing, hanged);

        Ok(())
    }

    fn process_block(&self, block: &BlockStuff, accepted: bool, applied: Option<&BlockIdExt>) -> Result<()> {

        log::trace!("process_block {}", block.id());

        block.block().read_extra()?.read_in_msg_descr()?.iterate_slices_with_keys(|key, _msg_slice| {
            if let Some(_) = self.messages.get(&key) {
                let (level, master_id, finalized) = if let Some(mc_id) = applied {
                    (RempMessageLevel::TonNode_RempMasterchain,
                     mc_id.clone(),
                     true)
                } else if accepted {
                    (RempMessageLevel::TonNode_RempShardchain,
                     BlockIdExt::default(),
                     false)
                } else {
                    (RempMessageLevel::TonNode_RempCollator,
                     BlockIdExt::default(),
                     false)
                };

                let status = RempMessageStatus::TonNode_RempAccepted (
                    ton_api::ton::ton_node::rempmessagestatus::RempAccepted {
                        level,
                        block_id: block.id().clone(),
                        master_id,
                    }
                );

                #[cfg(feature = "telemetry")] 
                if finalized {
                    let engine = self.engine.get().ok_or_else(|| error!("engine was not set"))?;
                    engine.remp_client_telemetry().set_message_finalized(&key);
                }

                self.new_self_processing_status(&key, status, finalized)?;
            }
            Ok(true)
        })?;

        Ok(())
    }

    async fn process_remp_receipt(&self, signed_receipt: RempSignedReceipt, source: &Arc<KeyId>) -> Result<()> {
        let engine = self.engine.get().ok_or_else(|| error!("engine was not set"))?;
        #[cfg(feature = "telemetry")]
        engine.remp_client_telemetry().register_got_receipt();
        #[cfg(feature = "telemetry")]
        let got_at = Instant::now();

        let receipt = ton_api::deserialize_boxed(&signed_receipt.receipt().0)?
            .downcast::<RempReceipt>().or_else(|_| fail!("Can't deserialise RempReceipt from TLObject"))?;
        
        self.check_receipt_signature(&signed_receipt, receipt.source_id()).await
            .map_err(|e| error!("Failed to check receipt's signature: {}", e))?;

        // TODO check is receipt's source one of shard's validators? So how? And is it need? 

        let message_id = receipt.message_id().clone();

        let _ = self.messages.get(&message_id)
            .ok_or_else(|| error!("Got receipt for unknown message with id {:x} from {}", message_id, source))?;

        let rejected = is_finally_rejected(receipt.status());
        let finalized = is_finally_accepted(receipt.status());
        let die_soon = rejected || finalized;
        #[cfg(feature = "telemetry")]
        if finalized {
            engine.remp_client_telemetry().set_message_finalized(&message_id);
        } else if rejected {
            engine.remp_client_telemetry().set_message_rejected(&message_id);
        }

        #[cfg(feature = "telemetry")]
        let status_short_name = remp_status_short_name(&receipt);

        let signature = signed_receipt.only().signature.0.try_into()
            .map_err(|_| error!("signed_receipt.signature has invalid length"))?;

        #[cfg(feature = "telemetry")]
        let rt = ReceiptTelemetry {
            status: status_short_name,
            got_at,
            processing_ns: got_at.elapsed().as_nanos() as u64,
            sending_ns: 0
        };
        self.new_processing_status(
            &message_id, 
            receipt, 
            signature, 
            die_soon, 
            #[cfg(feature = "telemetry")]
            Some(rt)
        )?;

        Ok(())
    }

    fn new_processing_history(&self, message_id: UInt256, mc_cc_to_die: u32) -> Result<()> {
        let engine = self.engine.get().ok_or_else(|| error!("engine was not set"))?;
        add_unbound_object_to_map_with_update(
            &self.messages,
            message_id.clone(),
            |found| if found.is_some() {
                fail!("Message {:x} is already in processing", message_id);
            } else {
                let s = RempMessageHistoryInternal {
                    last_update: AtomicU64::new(engine.now_ms()),
                    time_to_die: AtomicU64::new(0),
                    statuses: lockfree::map::Map::new(),
                    mc_cc_to_die,
                };
                Ok(Some(s))
            }
        )?;
        Ok(())
    }

    fn new_self_processing_status(
        &self,
        message_id: &UInt256,
        status: RempMessageStatus,
        die_soon: bool
    ) -> Result<()> {
        let engine = self.engine.get().ok_or_else(|| error!("engine was not set"))?;

        let receipt = ton_api::ton::ton_node::rempreceipt::RempReceipt {
            message_id: message_id.clone(),
            status,
            timestamp: engine.now_ms() as i64,
            source_id: self.local_key_id.clone()
        }.into_boxed();
        let signature = engine.sign_remp_receipt(&receipt)?;
        self.new_processing_status(
            message_id,
            receipt,
            signature,
            die_soon,
            #[cfg(feature = "telemetry")]
            None
        )
    }

    fn new_processing_status(
        &self,
        message_id: &UInt256,
        status: RempReceipt,
        signature: Vec<u8>,
        die_soon: bool,
        #[cfg(feature = "telemetry")]
        receipt_telemetry: Option<ReceiptTelemetry>
    ) -> Result<()> {
        
        log::info!("New processing stage for external message {:x}: {:?}, source: {:x}, die_soon: {}",
            message_id, status.status(), status.source_id(), die_soon);

        if let Some(msg) = self.messages.get(message_id) {

            let timestamp = *status.timestamp() as u64;
            msg.val().last_update.fetch_max(timestamp, Ordering::Relaxed);

            if let Some(status) = msg.val().statuses.insert(signature.clone(), status.clone()) {
                log::warn!("Duplicate of processing status for external message {:x}: {:?}, source: {:x}, die_soon: {}",
                    message_id, status.val().status(), status.val().source_id(), die_soon);
            }

            if die_soon {
                msg.val().time_to_die.store(timestamp + self.time_before_die_ms, Ordering::Relaxed);
            }
        }

        // Send to kafka (async)
        let engine = self.engine.get().ok_or_else(|| error!("engine was not set"))?.clone();
        let message_id = message_id.clone();
        tokio::spawn(async move {
            #[cfg(feature = "telemetry")]
            let sending = Instant::now();
            if let Err(e) = engine.process_remp_msg_status_in_ext_db(&message_id, &status, &signature).await {
                log::error!("Can't send status of {} to ext db: {}", message_id, e)
            } else {
                #[cfg(feature = "telemetry")] {
                    if let Some(mut rt) = receipt_telemetry {
                        rt.sending_ns = sending.elapsed().as_nanos() as u64;
                        engine.remp_client_telemetry().add_receipt(&message_id, rt);
                    }
                }
            }
        });
        Ok(())
    }

    async fn process_remp_message_impl(
        self: Arc<Self>,
        raw_message: Vec<u8>,
        id: UInt256,
    ) -> Result<()> {
        
        log::trace!("process_remp_message_impl {:x}", id);

        let engine = self.engine.get().ok_or_else(|| error!("engine was not set"))?;

        #[cfg(feature = "telemetry")]
        engine.remp_client_telemetry().register_got_message();
        #[cfg(feature = "telemetry")]
        let got_at = Instant::now();

        // TODO check if message is already in map
        if self.messages.get(&id).is_some() {
            fail!("message {:x} is already in processing", id);
        }

        if !engine.check_sync().await? {
            fail!("Can't process REMP message because node is out of sync");
        }

        let (real_id, message) = create_ext_message(&raw_message)?;
        if real_id != id {
            fail!("Given message id {:x} is not equal calculated one {:x}", id, real_id);
        }

        let dst_wc = message.dst_workchain_id()
            .ok_or_else(|| error!("Can't get workchain id from message"))?;

        let (process_mc, processed_wc) = engine.processed_workchain().await?;

        if !((dst_wc == processed_wc) || (process_mc && dst_wc == MASTERCHAIN_ID)) {
            let wc_descr = if dst_wc == MASTERCHAIN_ID {
                "masterchain".to_owned() 
            } else { 
                format!("workchain {}", dst_wc) 
            };
            fail!("The node doesn't prorecess {}", wc_descr);
        }

        let dst_address = message.int_dst_account_id()
            .ok_or_else(|| error!("Can't get standart destination address from message"))?;

        let (account, dst_shard) = engine.clone().load_account(dst_wc, dst_address.clone()).await?;

        let last_mc_state = engine.load_last_applied_mc_state().await?;
        if !self.skip_run_local {
            Self::run_local(&account, &last_mc_state, message, &id, engine.now()).await?;
        }

        // Build RempMessage struct
        let remp_message = RempMessage {
            message: raw_message.into(),
            id: id.into(), // 
            timestamp: 0, // Validator sets it?
            signature: Vec::new().into() // TODO
        };

        let (validators, mc_cc_to_die) = Self::calculate_validators(
            last_mc_state,
            dst_shard,
            dst_address,
            engine.now()
        )?;
        #[cfg(feature = "telemetry")]
        let processing_ns = got_at.elapsed().as_nanos() as u64;

        // add message to map (it is possible to receive status before will have be sent
        // to all validators, so we must have message in the map to handle it properly)
        self.new_processing_history(id.clone(), mc_cc_to_die)?;

        // Send to validators
        let total_validators = validators.len() as u32;
        match self.send_remp_message(validators, remp_message).await {
            Err(e) => {
                log::warn!("Remp message {} possibly was not sent to validators: {}", id, e);
                return Ok(())
            },
            Ok(sent_to) => {

                // When we already sent message we can't return error because error means "reject" 

                let status = RempMessageStatus::TonNode_RempSentToValidators (
                    ton_api::ton::ton_node::rempmessagestatus::RempSentToValidators {
                        sent_to: sent_to as i32,
                        total_validators: total_validators as i32
                    }
                );        
                if let Err(e) = self.new_self_processing_status(&id, status, false) {
                    log::error!("new_self_processing_status: {}", e);
                }
            },
        };

        #[cfg(feature = "telemetry")] {
            let sending_ns = got_at.elapsed().as_nanos() as u64;
            engine.remp_client_telemetry().register_sent_message();
            engine.remp_client_telemetry().add_message(
                id.clone(), got_at, processing_ns, sending_ns);
        }

        log::trace!("process_remp_message_impl {:x} done", id);

        Ok(())
    }

    fn calculate_validators(
        last_mc_state: Arc<ShardStateStuff>,
        shard: ShardIdent,
        address: AccountId,
        now: u32,
    ) -> Result<(HashSet<Arc<KeyId>>, u32)> {
        let last_mc_state_extra = last_mc_state.state().read_custom()?
            .ok_or_else(|| error!("State for {} doesn't have McStateExtra", last_mc_state.block_id()))?;
        let cc_config = last_mc_state_extra.config.catchain_config()?;
        let cur_validator_set = last_mc_state_extra.config.validator_set()?;
        let next_validator_set = last_mc_state_extra.config.next_validator_set()?;
        let next_set_lag = 30; // include next set if it is activating during

        if cur_validator_set.utime_until() < now + next_set_lag && next_validator_set.total() == 0 {
            log::warn!("Current validator set expires soon but new one is still empty!")
        }

        // In the vector we will collect paramaters to calculate all needed subsets
        let mut subset_params = vec!();
        
        if shard.is_masterchain() {

            // Masterchain - current set and next (if current expires soon)

            subset_params.push((
                shard.clone(),
                last_mc_state_extra.validator_info.catchain_seqno,
                &cur_validator_set
            ));
            if cur_validator_set.utime_until() < now + next_set_lag && next_validator_set.total() > 0 {
                subset_params.push((
                    shard.clone(),
                    last_mc_state_extra.validator_info.catchain_seqno + 1, // TODO is it correct?
                    &next_validator_set
                ));
            }
        } else {

            // possible reasons for a new set:
            // - current catchain expires according to CatchainConfig::shard_catchain_lifetime. 
            //   It happens at a strictly defined time (and we can calculate the time)
            // - current validator set expires.
            //   This event may be slightly delayed. We only know the lower bound (cur_validator_set.utime_until)
            // - split or merge
            //   - if before_split/before_merge flag set - split/merge is happening next block
            //   - FutureSplitMerge::Split/FutureSplitMerge::Merge don't give any garantees, 
            //     shard may split/merge any time in given period

            let cc_expires_at =  now - now % cc_config.shard_catchain_lifetime + cc_config.shard_catchain_lifetime;
            let shard_descr = last_mc_state_extra.shards().find_shard(&shard)?.ok_or_else(
                || error!("Can't find description for shard {} in state {}", shard, last_mc_state.block_id())
            )?.descr;

            let cc_seqno = last_mc_state_extra.shards().calc_shard_cc_seqno(&shard)?;

            let shards = if shard_descr.before_merge && shard_descr.before_split {
                fail!(
                    "In descr for shard {} 'before_merge' and 'before_split' flags are set both - it is incorrect (mc block {})",
                    shard, last_mc_state.block_id()
                )
            } else if !shard_descr.before_merge && !shard_descr.before_split {
                match shard_descr.split_merge_at {
                    FutureSplitMerge::None => vec!((shard.clone(), cc_seqno)),
                    FutureSplitMerge::Split{split_utime: time, interval: _interval} => {
                        if time < now + next_set_lag {
                            let (s1, s2) = shard.split()?;
                            let s = if s1.contains_account(address)? {
                                s1
                            } else {
                                s2
                            };
                            vec!((shard.clone(), cc_seqno), (s, cc_seqno + 1))
                        } else {
                            vec!((shard.clone(), cc_seqno))
                        }
                    }
                    FutureSplitMerge::Merge{merge_utime: time, interval: _interval} => {
                        if time < now + next_set_lag {
                            vec!((shard.clone(), cc_seqno), (shard.merge()?, cc_seqno + 1))
                        } else {
                            vec!((shard.clone(), cc_seqno))
                        } 
                    }
                }
            } else if shard_descr.before_merge {
                vec!((shard.merge()?, cc_seqno + 1))
            } else { // if shard_descr.before_split
                let (s1, s2) = shard.split()?;
                if s1.contains_account(address)? {
                    vec!((s1, cc_seqno + 1))
                } else {
                    vec!((s2, cc_seqno + 1))
                }
            };

            for (shard, cc_seqno) in shards {
                // Current set
                subset_params.push((
                    shard.clone(),
                    cc_seqno,
                    &cur_validator_set
                ));
                // Next catchain
                if cc_expires_at < now + next_set_lag {
                    subset_params.push((
                        shard.clone(),
                        cc_seqno + 1,
                        &cur_validator_set
                    ));
                }
                // Next val set
                if cur_validator_set.utime_until() < now + next_set_lag {
                    let cc_seqno = if cc_expires_at < cur_validator_set.utime_until() {
                        cc_seqno + 2
                    } else {
                        // it is prediction. If next validator set slightly delays we will need "+2" may be
                        cc_seqno + 1 
                    };
                    subset_params.push((
                        shard.clone(),
                        cc_seqno,
                        &next_validator_set
                    ));
                }
            }
        }

        let mut validators = HashSet::new();
        for (shard, cc_seqno, validator_set) in subset_params {
            let (subset, _hash_short) = validator_set.calc_subset(
                &cc_config, 
                shard.shard_prefix_with_tag(), 
                shard.workchain_id(), 
                cc_seqno,
                0.into())?;
            for v in &subset {
                validators.insert(get_adnl_id(v));
            }
        }

        let mc_cc_expires_at =  now - now % cc_config.mc_catchain_lifetime + cc_config.mc_catchain_lifetime;
        let mc_cc_to_die = last_mc_state_extra.validator_info.catchain_seqno + 
            if mc_cc_expires_at < now + next_set_lag {
                4
            } else {
                3
            };

        Ok((validators, mc_cc_to_die))
    }

    // TODO Move method to "remp helper"
    async fn run_local(
        account: &ShardAccount,
        last_mc_state: &ShardStateStuff,
        message: Message,
        id: &UInt256,
        block_utime: u32,
    ) -> Result<()> {
        log::trace!("run_local {:x}", id);

        let last_mc_state_extra = last_mc_state.state().read_custom()?
            .ok_or_else(|| error!("State for {} doesn't have McStateExtra", last_mc_state.block_id()))?;
        let config = BlockchainConfig::with_config(last_mc_state_extra.config)?;

        let params = ExecuteParams {
            state_libs: last_mc_state.state().libraries().clone().inner(),
            block_unixtime: block_utime,
            block_lt: 0, // ???
            last_tr_lt: Arc::new(AtomicU64::new(account.last_trans_lt())),
            seed_block: UInt256::default(),
            debug: false,
            ..ExecuteParams::default()
        };
        let now = Instant::now();
        let mut account_root = account.account_cell();
        let execution_result = tokio::task::spawn_blocking(move || {
            let executor = OrdinaryTransactionExecutor::new(config);
            executor.execute_with_libs_and_params(Some(&message), &mut account_root, params)
        }).await?.map(|_| ());

        let duration = now.elapsed().as_micros() as u64;
        log::trace!("run_local  message {:x}  account {}  TIME execute {}μ;",
            id, account.read_account()?.get_addr().cloned().unwrap_or_default(), duration);

        execution_result
    }

    async fn send_remp_message(
        &self,
        validators: HashSet<Arc<KeyId>>,
        message: RempMessage,
    ) -> Result<u32> {
        log::trace!("send_remp_message {}", message.id);

        let engine = self.engine.get().ok_or_else(|| error!("engine was not set"))?;
        let mut send_futures = vec![];
        for validator in &validators {
            send_futures.push(
                engine.send_remp_message(validator.clone(), message.clone().into_boxed()), // how not to clone?
            ); 
        }
        
        let total = validators.len();
        let mut succeded = 0_u32;
        for (result, validator) in futures::future::join_all(send_futures).await.into_iter().zip(validators.iter()) {
            match result {
                Err(e) => log::warn!("error while sending remp message {:x} to validator {}: {}", 
                                message.id, validator, e),
                Ok(_) => {
                    log::debug!("Message {:x} was sucessfully sent to {}", message.id, validator);
                    succeded += 1;
                }
            }
        }

        if succeded == 0 {
            fail!("No one successfully sent remp message")
        } else {
            log::trace!("send_remp_message {:x} send to {} of {}", message.id, succeded, total);
            Ok(succeded)
        }
    }

    async fn resolve_validators(&self) -> Result<(BlockIdExt, HashSet<ValidatorDescr>)> {

        log::trace!("resolve_validators");

        let engine = self.engine.get().ok_or_else(|| error!("engine was not set"))?;

        // init validator sets using last master state and resolve current (and possibly next) validators sets
        
        let last_mc_state = engine.load_last_applied_mc_state_or_zerostate().await?;
        let last_mc_state_extra = last_mc_state.state().read_custom()?
            .ok_or_else(|| error!("State for {} doesn't have McStateExtra", last_mc_state.block_id()))?;
        let mut to_resolve = vec!();
        let cur_vset = last_mc_state_extra.config.validator_set()?;
        let next_vset = last_mc_state_extra.config.next_validator_set()?;
        for v in cur_vset.list().iter().chain(next_vset.list().iter()) {
            to_resolve.push(validatordescr_to_catchain_node(v));
        }
         // TODO support callback
        engine.update_validators(to_resolve, vec!()).await?;

        let mut resolved = HashSet::new();
        for v in cur_vset.list().iter().chain(next_vset.list().iter()) {
            resolved.insert(v.clone());
        }

        log::trace!("resolve_validators  block {}  done", last_mc_state.block_id());
        Ok((
            last_mc_state.block_id().clone(),
            resolved
        ))
    }

    async fn resolve_validators_if_need(
        &self,
        block: &BlockStuff,
        validators: &mut HashSet<ValidatorDescr>
    ) -> Result<()> {
        if block.block().read_info()?.key_block() {

            log::trace!("resolve_validators  key block {}", block.id());

            let custom = block
                .block()
                .read_extra()?
                .read_custom()?
                .ok_or_else(|| error!("Can't load custom from block {}", block.id()))?;
            let config = custom
                .config()
                .ok_or_else(|| error!("Can't load config from block {}", block.id()))?;
            let cur_vset = config.validator_set()?;
            let next_vset = config.next_validator_set()?;
            let mut new = HashSet::new();
            for v in cur_vset.list().iter().chain(next_vset.list().iter()) {
                new.insert(v.clone());
            }
            let mut to_delete = vec!();
            for v in validators.iter() {
                if !new.contains(v) {
                    to_delete.push(validatordescr_to_catchain_node(v))
                }
            }
            let mut to_resolve = vec!();
            for v in &new {
                if !validators.contains(v) {
                    to_resolve.push(validatordescr_to_catchain_node(v))
                }
            }
            if to_delete.len() > 0 || to_resolve.len() > 0 {
                let engine = self.engine.get().ok_or_else(|| error!("engine was not set"))?;
                 // TODO support callback
                engine.update_validators(to_resolve, to_delete).await?;
                *validators = new;
                log::trace!("resolve_validators  key block {}  done", block.id());
            } else {
                log::trace!("resolve_validators  no one change  key block {}", block.id());
            }
        }
        Ok(())
    }

    async fn check_receipt_signature(
        &self,
        signed_receipt: &RempSignedReceipt,
        source_key_id: &UInt256,
    ) -> Result<()> {
        // get latest mc state
        let engine = self.engine.get().ok_or_else(|| error!("engine was not set"))?;
        let last_mc_state = engine.load_last_applied_mc_state().await?;

        // find validator with needed pub key id
        let cur_validator_set = last_mc_state.config_params()?.validator_set()?;
        let mut pub_key = None;
        for v in cur_validator_set.list() {
            let k = Ed25519KeyOption::from_public_key(v.public_key.key_bytes());
            if k.id().data() == source_key_id.as_slice() {
                pub_key = Some(k);
                break;
            }
        }
        let pub_key = pub_key.ok_or_else(|| {
            error!("Can't find validator with key id {} in the current set", source_key_id)
        })?;

        // check receipt's signature
        pub_key.verify(signed_receipt.receipt(), signed_receipt.signature())
    }
}

pub fn remp_status_short_name(
    status: &RempReceipt,
) -> String {
    match status.status() {
        RempMessageStatus::TonNode_RempAccepted(acc) => {
            match acc.level {
                RempMessageLevel::TonNode_RempCollator => "Accepted_Collator",
                RempMessageLevel::TonNode_RempFullnode => "Accepted_Fullnode",
                RempMessageLevel::TonNode_RempMasterchain => "Accepted_Masterchain",
                RempMessageLevel::TonNode_RempQueue => "Accepted_Queue",
                RempMessageLevel::TonNode_RempShardchain => "Accepted_Shardchain",
            }
        },
        RempMessageStatus::TonNode_RempDuplicate(_) => "Duplicate",
        RempMessageStatus::TonNode_RempIgnored(ign) => {
            match ign.level {
                RempMessageLevel::TonNode_RempCollator => "Ignored_Collator",
                RempMessageLevel::TonNode_RempFullnode => "Ignored_Fullnode",
                RempMessageLevel::TonNode_RempMasterchain => "Ignored_Masterchain",
                RempMessageLevel::TonNode_RempQueue => "Ignored_Queue",
                RempMessageLevel::TonNode_RempShardchain => "Ignored_Shardchain",
            }
        },
        RempMessageStatus::TonNode_RempNew => "New",
        RempMessageStatus::TonNode_RempRejected(rj) => {
            match rj.level {
                RempMessageLevel::TonNode_RempCollator => "Rejected_Collator",
                RempMessageLevel::TonNode_RempFullnode => "Rejected_Fullnode",
                RempMessageLevel::TonNode_RempMasterchain => "Rejected_Masterchain",
                RempMessageLevel::TonNode_RempQueue => "Rejected_Queue",
                RempMessageLevel::TonNode_RempShardchain => "Rejected_Shardchain",
            }
        },
        RempMessageStatus::TonNode_RempSentToValidators(_) => "SentToValidators",
        RempMessageStatus::TonNode_RempTimeout => "Timeout",
    }.to_owned()
}

