use std::{
    collections::{HashMap, HashSet},
    convert::TryFrom,
    ops::Deref,
    sync::Arc,
    time::{Duration, SystemTime}
};
use crate::{
    engine::STATSD,
    engine_traits::EngineOperations,
    shard_state::ShardStateStuff,
    validator::{
        validator_group::{ValidatorGroup, ValidatorGroupStatus},
        validator_utils::{
            calc_subset_for_workchain,
            validatordescr_to_catchain_node,
            get_shard_name, validatorset_to_string,
            compute_validator_list_id,
            ValidatorListHash
        },
    },
};
#[cfg(feature = "slashing")]
use crate::validator::slashing::{SlashingManager, SlashingManagerPtr};
use catchain::{CatchainNode, PublicKey};
use catchain::utils::serialize_tl_boxed_object;
use tokio::{time::timeout, runtime::Runtime};
use ton_api::IntoBoxed;
use ton_block::{
    BlockIdExt, CatchainConfig, ConfigParamEnum, ConsensusConfig, 
    McStateExtra, ShardIdent, ValidatorDescr, ValidatorSet,
    FutureSplitMerge, ShardDescr,
};
use ton_types::{error, fail, Result, UInt256};

fn get_validator_set_id_serialize(
    shard: &ShardIdent,
    val_set: &ValidatorSet,
    opts_hash: &UInt256,
    key_seqno: i32,
    new_catchain_ids: bool,
    max_vertical_seqno: i32,
) -> catchain::RawBuffer {
    let members = val_set.list().iter().map(|descr| {
        let node_id = descr.compute_node_id_short();
        let adnl_id = descr.adnl_addr.clone().unwrap_or(node_id.clone());
        ton_api::ton::engine::validator::validator::groupmember::GroupMember {
            public_key_hash: ton_api::ton::int256(node_id.inner()),
            adnl: ton_api::ton::int256(adnl_id.inner()),
            weight: descr.weight as i64,
        }
    }).collect::<Vec<_>>();

    if !new_catchain_ids {
        unimplemented!("Old catchain ids format is not supported")
    } else {
        serialize_tl_boxed_object!(&ton_api::ton::validator::group::GroupNew {
            workchain: shard.workchain_id(),
            shard: shard.shard_prefix_with_tag() as i64,
            vertical_seqno: max_vertical_seqno,
            last_key_block_seqno: key_seqno,
            catchain_seqno: val_set.catchain_seqno() as i32,
            config_hash: ton_api::ton::int256(*opts_hash.as_slice()),
            members: members.into()
        }
        .into_boxed())
    }
}

/// serialize data and calc sha256
fn get_validator_set_id(
    shard: &ShardIdent,
    val_set: &ValidatorSet,
    opts_hash: &UInt256,
    key_seqno: i32,
    new_catchain_ids: bool,
    max_vertical_seqno: i32,
) -> UInt256 {
    let serialized = get_validator_set_id_serialize(
        shard,
        val_set,
        opts_hash,
        key_seqno,
        new_catchain_ids,
        max_vertical_seqno,
    );
    UInt256::calc_file_hash(&serialized.0)
}

fn validator_session_options_serialize(
    opts: &validator_session::SessionOptions,
) -> catchain::RawBuffer {
    serialize_tl_boxed_object!(&ton_api::ton::validator_session::config::ConfigNew {
        catchain_idle_timeout: opts.catchain_idle_timeout.as_secs_f64().into(),
        catchain_max_deps: i32::try_from(opts.catchain_max_deps).unwrap(),
        round_candidates: i32::try_from(opts.round_candidates).unwrap(),
        next_candidate_delay: opts.next_candidate_delay.as_secs_f64().into(),
        round_attempt_duration: i32::try_from(opts.round_attempt_duration.as_secs()).unwrap(),
        max_round_attempts: i32::try_from(opts.max_round_attempts).unwrap(),
        max_block_size: i32::try_from(opts.max_block_size).unwrap(),
        max_collated_data_size: i32::try_from(opts.max_collated_data_size).unwrap(),
        new_catchain_ids: ton_api::ton::Bool::from(opts.new_catchain_ids)
    }
    .into_boxed())
}

fn get_validator_session_options_hash(opts: &validator_session::SessionOptions) -> (UInt256, catchain::RawBuffer) {
    let serialized = validator_session_options_serialize(opts);
    (UInt256::calc_file_hash(&serialized.0), serialized)
}

fn get_session_options(opts: &ConsensusConfig) -> validator_session::SessionOptions {
    validator_session::SessionOptions {
        catchain_idle_timeout: std::time::Duration::from_millis(opts.consensus_timeout_ms.into()),
        catchain_max_deps: opts.catchain_max_deps,
        catchain_skip_processed_blocks: false, // Debugging option, not found in consensus config
        round_candidates: opts.round_candidates,
        next_candidate_delay: std::time::Duration::from_millis(opts.next_candidate_delay_ms.into()),
        round_attempt_duration: std::time::Duration::from_secs(opts.attempt_duration.into()),
        max_round_attempts: opts.fast_attempts,
        max_block_size: opts.max_block_bytes,
        max_collated_data_size: opts.max_collated_bytes,
        new_catchain_ids: opts.new_catchain_ids,
    }
}

struct ValidatorManagerConfig {
    update_interval: Duration
}

impl Default for ValidatorManagerConfig {
    fn default() -> Self {
        return ValidatorManagerConfig {
            update_interval: Duration::from_secs(3)
        }
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Debug)]
enum ValidationStatus { Disabled, Waiting, Countdown, Active }

impl ValidationStatus {
    fn allows_validate(&self) -> bool {
        match self {
            Self::Disabled | Self::Waiting => false,
            Self::Countdown | Self::Active => true
        }
    }
}

struct ValidatorListStatus {
    known_lists: HashMap<ValidatorListHash, PublicKey>,
    curr: Option<ValidatorListHash>,
    next: Option<ValidatorListHash>
}

impl ValidatorListStatus {
    fn add_list (&mut self, list_id: ValidatorListHash, key: PublicKey) {
        self.known_lists.insert(list_id, key);
    }

    fn contains_list (&self, list_id: &ValidatorListHash) -> bool {
        return self.known_lists.contains_key(list_id);
    }

    fn remove_list (&mut self, list_id: &ValidatorListHash) {
        self.known_lists.remove(list_id);
    }

    fn get_list (&self, list_id: &ValidatorListHash) -> Option<PublicKey> {
        return match self.known_lists.get(list_id) {
            None => None,
            Some(ch) => Some(ch.clone())
        }
    }

    fn get_local_key (&self) -> Option<PublicKey> {
        return match &self.curr {
            None => None,
            Some(ch) => self.get_list (&ch)
        }
    }

    fn actual_or_coming (&self, list_id: &ValidatorListHash) -> bool {
        match &self.curr {
            Some(curr_id) if list_id == curr_id => return true,
            _ => ()
        };

        match &self.next {
            Some(next_id) if list_id == next_id => return true,
            _ => return false
        }
    }

    fn known_hashes (&self) -> HashSet<ValidatorListHash> {
        return self.known_lists.keys().cloned().collect();
    }
}

impl Default for ValidatorListStatus {
    fn default() -> ValidatorListStatus {
        return ValidatorListStatus {
            known_lists: HashMap::default(),
            curr: None,
            next: None
        }
    }
}

fn rotate_all_shards(mc_state_extra: &McStateExtra) -> bool {
    mc_state_extra.validator_info.nx_cc_updated
}

struct ValidatorManagerImpl {
    engine: Arc<dyn EngineOperations>,
    rt: Arc<Runtime>,
    validator_sessions: HashMap<UInt256, Arc<ValidatorGroup>>, // Sessions: both actual (started) and future
    validator_list_status: ValidatorListStatus,
    config: ValidatorManagerConfig,
    #[cfg(feature = "slashing")]
    slashing_manager: SlashingManagerPtr,
    validation_status: ValidationStatus,
}

// struct ValidatorManagerData {

// }

impl ValidatorManagerImpl {

    fn new(engine: Arc<dyn EngineOperations>) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_stack_size(8 * 1024 * 1024)
            .build()
            .expect("Can't create validator groups runtime");

        ValidatorManagerImpl {
            engine,
            rt: Arc::new(rt),
            validator_sessions: HashMap::default(),
            validator_list_status: ValidatorListStatus::default(),
            config: ValidatorManagerConfig::default(),
            validation_status: ValidationStatus::Disabled,
            #[cfg(feature = "slashing")]
            slashing_manager: SlashingManager::create(),
        }
    }

    /// find own key in validator subset
    fn find_us(&self, validators: &[ValidatorDescr]) -> Option<PublicKey> {
        if let Some(lk) = self.validator_list_status.get_local_key() {
            let local_keyhash = lk.id().data();
            for val in validators {
                let pkhash = val.compute_node_id_short();
                if pkhash.as_slice() == local_keyhash {
                    //log::info!(target: "validator", "Comparing {} with {}", pkhash, local_keyhash);
                    //log::info!(target: "validator", "({:?})", pk.pub_key().unwrap());
                    //compute public key hash
                    return Some(lk)
                }
            }
        }
        None
    }

    async fn update_single_validator_list(&mut self, validator_list: &[ValidatorDescr], name: &str)
    -> Result<Option<ValidatorListHash>> {
        let list_id = match compute_validator_list_id(validator_list) {
            None => return Ok(None),
            Some(l) if self.validator_list_status.contains_list(&l) => return Ok(Some(l)),
            Some(l) => l,
        };

        let nodes_res: Vec<CatchainNode> = validator_list
            .iter()
            .map(validatordescr_to_catchain_node)
            .collect::<Result<Vec<CatchainNode>>>()?;

        log::info!(target: "validator", "Updating {} validator list (id {:x}):", name, list_id);
        for x in &nodes_res {
            log::info!(target: "validator", "pk: {}, pk_id: {}, andl_id: {}",
                hex::encode(x.public_key.pub_key().unwrap()),
                hex::encode(x.public_key.id().data()),
                hex::encode(x.adnl_id.data())
            );
        }

        match self.engine.set_validator_list(list_id.clone(), &nodes_res).await? {
            Some(key) => {
                self.validator_list_status.add_list(list_id.clone(), key.clone());
                log::info!(target: "validator", "Local node: pk_id: {} id: {}",
                    hex::encode(key.pub_key().unwrap()),
                    hex::encode(key.id().data())
                );
                return Ok(Some(list_id));
            },
            None => {
                log::info!(target: "validator", "Local node is not a {} validator", name);
                return Ok(None);
            }
        }
    }

    async fn update_validator_lists(&mut self, mc_state: &ShardStateStuff) -> Result<bool> {
        let (validator_set, next_validator_set) = match mc_state.state().read_custom()? {
            None => return Ok(false),
            Some(state) => (state.config.validator_set()?, state.config.next_validator_set()?)
        };

        self.validator_list_status.curr = self.update_single_validator_list(validator_set.list(), "current").await?;
        if let Some(id) = self.validator_list_status.curr.as_ref() {
            self.engine.activate_validator_list(id.clone())?;
        }
        self.validator_list_status.next = self.update_single_validator_list(next_validator_set.list(), "next").await?;

        STATSD.gauge("in_current_vset_p34", if self.validator_list_status.curr.is_some() { 1 } else { 0 } as f64);
        STATSD.gauge("in_next_vset_p36", if self.validator_list_status.next.is_some() { 1 } else { 0 } as f64);
        return Ok(!self.validator_list_status.curr.is_none() || !self.validator_list_status.next.is_none());
    }

    async fn garbage_collect_lists(&mut self) -> Result<()> {
        log::trace!(target: "validator", "Garbage collect lists");
        let mut lists_gc = self.validator_list_status.known_hashes();

        for id in self.validator_sessions.values().into_iter() {
            lists_gc.remove(&id.get_validator_list_id());
        }

        for id in lists_gc {
            if !self.validator_list_status.actual_or_coming (&id) {
                log::trace!(target: "validator", "Removing validator list: {:x}", id);
                self.validator_list_status.remove_list(&id);
                self.engine.remove_validator_list(id.clone()).await?;
                log::trace!(target: "validator", "Validator list removed: {:x}", id);
            } else {
                log::trace!(target: "validator", "Validator list is still actual: {:x}", id);
            }
        }
        log::trace!(target: "validator", "Garbage collect lists -- ok");

        Ok(())
    }

    async fn stop_and_remove_sessions(&mut self, sessions_to_remove: &HashSet<UInt256>) {
        for id in sessions_to_remove.iter() {
            log::trace!(target: "validator", "stop&remove: removing {:x}", id);
            match self.validator_sessions.get(id) {
                None => {
                    log::error!(target: "validator",
                        "Session stopping error: {:x} already removed from hash", id
                    )
                }
                Some(session) => {
                    match session.get_status().await {
                        ValidatorGroupStatus::Stopping => {}
                        ValidatorGroupStatus::Stopped => {
                            if let Some(group) = self.validator_sessions.remove(id) {
                                self.engine.validation_status().remove(group.shard());
                                self.engine.collation_status().remove(group.shard());
                            }
                        }
                        _ => {
                            if let Err(e) = session.clone().stop(self.rt.clone()).await {
                                log::error!(target: "validator",
                                    "Could not stop session {:x}: `{}`", id, e);
                                    self.validator_sessions.remove(id);
                            }
                        }
                    }
                }
            }
        }
    }

    async fn compute_session_options(&mut self, mc_state_extra: &McStateExtra)
    -> Result<(validator_session::SessionOptions, UInt256)> {
        let consensus_config = match mc_state_extra.config.config(29)? {
            Some(ConfigParamEnum::ConfigParam29(ccc)) => ccc.consensus_config,
            _ => fail!("no CatchainConfig in config_params"),
        };
        let session_options = get_session_options(&consensus_config);
        let (opts_hash, session_options_serialized) = get_validator_session_options_hash(&session_options);
        log::info!(target: "validator", "SessionOptions from config.29: {:?}", session_options);
        log::debug!(
            target: "validator",
            "SessionOptions from config.29 serialized: {} hash: {:x}",
            hex::encode(session_options_serialized.0),
            opts_hash
        );
        Ok((session_options, opts_hash))
    }

    async fn update_validation_status(&mut self, mc_state: &ShardStateStuff, mc_state_extra: &McStateExtra) -> Result<()> {
        match self.validation_status {
            ValidationStatus::Waiting => {
                let rotate = rotate_all_shards(mc_state_extra);
                let last_masterchain_block = mc_state.block_id();
                let later_than_hardfork = self.engine.get_last_fork_masterchain_seqno() <= last_masterchain_block.seq_no;
                if self.engine.check_sync().await?
                && (rotate || last_masterchain_block.seq_no == 0)
                && later_than_hardfork {
                    self.validation_status = ValidationStatus::Countdown
                }
            }
            ValidationStatus::Countdown => {
                for (_, group) in self.validator_sessions.iter() {
                    if group.get_status().await == ValidatorGroupStatus::Active {
                        self.validation_status = ValidationStatus::Active;
                        break
                    }
                }
            }
            ValidationStatus::Disabled | ValidationStatus::Active => {}
        }
        Ok(())
    }

    async fn disable_validation(&mut self) -> Result<()> {
        self.validation_status = ValidationStatus::Disabled;

        let existing_validator_sessions: HashSet<UInt256> =
            self.validator_sessions.keys().cloned().collect();
        self.stop_and_remove_sessions(&existing_validator_sessions).await;
        self.engine.set_will_validate(false);
        self.engine.clear_last_rotation_block_id()?;
        log::info!(target: "validator", "All sessions were removed, validation disabled");
        Ok(())
    }

    fn enable_validation(&mut self) {
        self.engine.set_will_validate(true);
        self.validation_status = std::cmp::max(self.validation_status, ValidationStatus::Waiting);
        log::info!(target: "validator", "Validation enabled: status {:?}", self.validation_status);
    }

    async fn start_sessions(
        &mut self,
        mut new_shards: HashMap<ShardIdent, Vec<BlockIdExt>>,
        keyblock_seqno: u32,
        session_options: validator_session::SessionOptions,
        opts_hash: &UInt256,
        catchain_config: &CatchainConfig,
        gc_validator_sessions: &mut HashSet<UInt256>,
        mc_now: u32,
        mc_state_extra: &McStateExtra,
        last_masterchain_block: &BlockIdExt
    ) -> Result<()> {
        let validator_list_id = match &self.validator_list_status.curr {
            Some(list_id) => list_id,
            None => return Ok(())
        };
        let full_validator_set = mc_state_extra.config.validator_set()?;

        let group_start_status = if self.validation_status == ValidationStatus::Countdown {
            let session_lifetime = std::cmp::min(catchain_config.mc_catchain_lifetime,
                                                 catchain_config.shard_catchain_lifetime);
            let start_at = tokio::time::Instant::now() + Duration::from_secs((session_lifetime/2).into());
            ValidatorGroupStatus::Countdown { start_at }
        } else {
            ValidatorGroupStatus::Active
        };

        for (ident, prev_blocks) in new_shards.drain() {
            let shard_name = get_shard_name(&ident);

            let cc_seqno_from_state = if ident.is_masterchain() {
                mc_state_extra.validator_info.catchain_seqno
            } else {
                mc_state_extra.shards().calc_shard_cc_seqno(&ident)?
            };

            let cc_seqno_delta = cc_seqno_from_state;
            let subset = calc_subset_for_workchain(
                &full_validator_set,
                &mc_state_extra.config,
                &catchain_config,
                ident.shard_prefix_with_tag(),
                ident.workchain_id(),
                cc_seqno_delta,
                mc_now.into(),
            )?;

            if let Some(local_id) = self.find_us(&subset.0) {
                let vsubset = ValidatorSet::with_cc_seqno(0, 0, 0, cc_seqno_delta, subset.0)?;

                let session_id = get_validator_set_id(
                    &ident,
                    &vsubset,
                    opts_hash,
                    keyblock_seqno as i32,
                    true,
                    0, /* temp */
                );

                log::info!(target: "validator", "subset for session: Shard {}, cc_seqno {}, keyblock_seqno {}, validator_set {}, session_id {:x}",
                    shard_name, cc_seqno_delta, keyblock_seqno,
                    validatorset_to_string(&vsubset), session_id
                );

                gc_validator_sessions.remove(&session_id);

                let engine = self.engine.clone();
                #[cfg(feature = "slashing")]
                let slashing_manager = self.slashing_manager.clone();
                let session = self.validator_sessions.entry(session_id.clone()).or_insert_with(|| 
                    Arc::new(ValidatorGroup::new(
                        ident.clone(),
                        local_id,
                        session_id,
                        validator_list_id.clone(),
                        vsubset,
                        session_options,
                        engine,
                        false,
                        #[cfg(feature = "slashing")]
                        slashing_manager,
                    ))
                );
                let session_status = session.get_status().await;
                if session_status == ValidatorGroupStatus::Created {
                    ValidatorGroup::start_with_status(
                        session.clone(),
                        group_start_status,
                        prev_blocks,
                        last_masterchain_block.clone(),
                        SystemTime::UNIX_EPOCH + Duration::from_secs(mc_now.into()),
                        self.rt.clone()
                    ).await?;
                } else if session_status >= ValidatorGroupStatus::Stopping {
                    log::error!(target: "validator", "Cannot start stopped session {}", session.info().await);
                }
            }
        }
        Ok(())
    }

    async fn update_shards(&mut self, mc_state: ShardStateStuff) -> Result<()> {
        if !self.update_validator_lists(&mc_state).await? {
            log::info!("Current validator list is empty, validation is disabled.");
            self.disable_validation().await?;
            return Ok(())
        }

        let mc_state_extra = mc_state.state().read_custom()?.expect("masterchain state must contain extra info");
        let last_masterchain_block = mc_state.block_id();

        let keyblock_seqno = if mc_state_extra.after_key_block {
            mc_state.block_id().seq_no
        } else {
            mc_state_extra.last_key_block.as_ref().map(|id| id.seq_no).expect("masterchain state must contain info about previous key block")
        };
        let mc_now = mc_state.state().gen_time();
        let (session_options, opts_hash) = self.compute_session_options(&mc_state_extra).await?;
        let catchain_config = mc_state_extra.config.catchain_config()?;

        self.enable_validation();
        self.update_validation_status(&mc_state, &mc_state_extra).await?;

        // Collect info about shards
        let mut gc_validator_sessions: HashSet<UInt256> =
            self.validator_sessions.keys().cloned().collect();

        // Shards that are about to start (continue) in this masterstate: shard_ident -> prevs
        let mut new_shards = HashMap::new();

        // Shards that will eventually be started (in later masterstates): need to prepare
        let mut future_shards: HashSet<ShardIdent> = HashSet::new();

        let (master, workchain_id) = self.engine.processed_workchain().await?;
        if !master {
            mc_state_extra.shards().iterate_shards_for_workchain(workchain_id, |ident: ShardIdent, descr: ShardDescr| {
                // Add all shards that are effective from now
                // ValidatorGroups will be created and appropriate sessions started for these shards
                let top_block = BlockIdExt::with_params(
                    ident.clone(),
                    descr.seq_no,
                    descr.root_hash,
                    descr.file_hash
                );

                if descr.before_split {
                    let lr_shards = ident.split();
                    match lr_shards {
                        Err(e) => log::error!(target: "validator", "Cannot split shard: `{}`", e),
                        Ok((l,r)) => {
                            new_shards.insert(l, vec![top_block.clone()]);
                            new_shards.insert(r, vec![top_block]);
                        }
                    }
                } else if descr.before_merge {
                    let parent_shard = ident.merge();
                    match parent_shard {
                        Err(e) => log::error!(target: "validator", "Cannot merge shard: `{}`", e),
                        Ok(p) => {
                            let mut prev_blocks = match new_shards.get(&p) {
                                Some(pb) => pb.clone(),
                                None => vec![BlockIdExt::default(), BlockIdExt::default()]
                            };

                            // Add previous block for the shard: there are two parents for merge, so two prevs
                            let (_l,r) = p.split()?;
                            prev_blocks[(r == ident) as usize] = top_block;
                            new_shards.insert(p, prev_blocks);
                        }
                    }
                } else {
                    new_shards.insert(ident, vec![top_block]);
                }

                // Create list of shards which will be effective soon
                // ValidatorGroups will be created for these shards, but not started.
                let cur_time = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?.as_secs();
                match descr.split_merge_at {
                    FutureSplitMerge::None => {
                        future_shards.insert(ident);
                    }
                    FutureSplitMerge::Split{split_utime: time, interval: _interval} => {
                        if (time as u64) < cur_time + 60 {
                            match ident.split() {
                                Ok((l,r)) => {
                                    future_shards.insert(l);
                                    future_shards.insert(r);
                                }
                                Err(e) => log::error!(target: "validator", "Cannot split shard {}: `{}`", ident, e)
                            }
                        } else {
                            future_shards.insert(ident);
                        }
                    }
                    FutureSplitMerge::Merge{merge_utime: time, interval: _interval} => {
                        if (time as u64) < cur_time + 60 {
                            match ident.merge() {
                                Ok(p) => {
                                    future_shards.insert(p);
                                }
                                Err(e) => log::error!(target: "validator", "Cannot merge shard {}: `{}`", ident, e)
                            }
                        } else {
                            future_shards.insert(ident);
                        }
                    }
                };

                Ok(true)
            })?;
        } else {
            new_shards.insert(ShardIdent::masterchain(), vec![last_masterchain_block.clone()]);
            future_shards.insert(ShardIdent::masterchain());
        }

        // Iterate over shards and start all missing sessions
        if self.validation_status.allows_validate() {
            self.start_sessions(new_shards, keyblock_seqno, session_options,
                                &opts_hash, &catchain_config, &mut gc_validator_sessions,
                                mc_now, &mc_state_extra, last_masterchain_block).await?;
        }

        // Initializing future shards
        log::info!(target: "validator", "Future shards initialization:");
        let next_validator_set = mc_state_extra.config.next_validator_set()?;
        let full_validator_set = mc_state_extra.config.validator_set()?;
        let possible_validator_change = next_validator_set.total() > 0;

        for ident in future_shards.iter() {
            let (cc_seqno_from_state, cc_lifetime) = if ident.is_masterchain() {
                (mc_state_extra.validator_info.catchain_seqno, catchain_config.mc_catchain_lifetime)
            } else {
                (mc_state_extra.shards().calc_shard_cc_seqno(&ident)?, catchain_config.shard_catchain_lifetime)
            };

            let near_validator_change = possible_validator_change &&
                next_validator_set.utime_since() <= (mc_now / cc_lifetime + 1) * cc_lifetime;
            let future_validator_set = if near_validator_change {
                log::info!(target: "validator", "Validator change will happen during catchain session lifetime for shard {}: cc_lifetime {}, now {}, next set since {}",
                    ident, cc_lifetime, mc_now, next_validator_set.utime_since());
                &next_validator_set
            } else {
                &full_validator_set
            };
            let next_subset = calc_subset_for_workchain(
                &future_validator_set,
                &mc_state_extra.config,
                &catchain_config,
                ident.shard_prefix_with_tag(),
                ident.workchain_id(),
                cc_seqno_from_state + 1,
                mc_now.into(),
            )?;

            if let Some(local_id) = self.find_us(&next_subset.0) {
                let vnext_subset = ValidatorSet::with_cc_seqno(0, 0, 0, 1, next_subset.0)?;
                let session_id = get_validator_set_id(
                    &ident,
                    &vnext_subset,
                    &opts_hash,
                    keyblock_seqno as i32,
                    true,
                    0, /* temp */
                );
                gc_validator_sessions.remove(&session_id);
                if !self.validator_sessions.contains_key(&session_id) {
                    if let Some(vnext_list_id) = compute_validator_list_id(&future_validator_set.list()) {
                        let session = Arc::new(ValidatorGroup::new(
                            ident.clone(),
                            local_id,
                            session_id.clone(),
                            vnext_list_id,
                            vnext_subset,
                            session_options,
                            self.engine.clone(),
                            false,
                            #[cfg(feature = "slashing")]
                            self.slashing_manager.clone(),
                        ));
                        self.validator_sessions.insert(session_id, session);
                    }
                }
            }
        }

        if rotate_all_shards(&mc_state_extra) {
            log::info!(target: "validator", "New last rotation block: {}", last_masterchain_block);
            self.engine.set_last_rotation_block_id(last_masterchain_block)?;
        }
        log::trace!(target: "validator", "starting stop&remove");
        self.stop_and_remove_sessions(&gc_validator_sessions).await;
        log::trace!(target: "validator", "starting garbage collect");
        self.garbage_collect_lists().await?;
        log::trace!(target: "validator", "exiting");
        Ok(())
    }

    async fn stats(&mut self) {
        log::info!(target: "validator", "{:32} {}", "session id", "st round shard");
        log::info!(target: "validator", "{:-64}", "");

        // Validation shards statistics
        for (_, group) in self.validator_sessions.iter() {
            log::info!(target: "validator", "{}", group.info().await);
            let status = group.get_status().await;
            if status == ValidatorGroupStatus::Active || status == ValidatorGroupStatus::Stopping {
                self.engine.validation_status().insert(group.shard().clone(), group.last_validation_time());
                self.engine.collation_status().insert(group.shard().clone(), group.last_collation_time());
            }
        }

        log::info!(target: "validator", "{:-64}", "");
    }

    /// infinte loop with possible error cancelation
    async fn invoke(&mut self) -> Result<()> {
        let mc_block_id = if let Some(id) = self.engine.get_last_rotation_block_id()? {
            log::info!(
                target: "validator", 
                "Validator manager initialization: last rotation block: {}",
                id
            );
            id
        } else if let Some(id) = self.engine.load_last_applied_mc_block_id()? {
            log::info!(
                target: "validator",
                "Validator manager initialization: last applied block: {}, no last rotation block",
                id
            );
            id.deref().clone()
        } else {
            fail!("Validator manager initialization neither last rotation nor applied block")
        };
        let mut mc_handle = self.engine.load_block_handle(&mc_block_id)?.ok_or_else(
            || error!("Cannot load handle for master block {}", mc_block_id)
        )?;
        loop {
            let mc_state = self.engine.load_state(mc_handle.id()).await?;
            log::info!(target: "validator", "Processing masterblock {}", mc_handle.id().seq_no);
            #[cfg(feature = "slashing")]
            if let Some(local_id) = self.validator_list_status.get_local_key() {
                log::debug!(target: "validator", "Processing slashing masterblock {}", mc_handle.id().seq_no);
                self.slashing_manager.handle_masterchain_block(&mc_handle, &mc_state, &local_id, &self.engine).await;
            }
            self.update_shards(mc_state).await?;
            
            mc_handle = loop {
                self.stats().await;
                match timeout(self.config.update_interval, self.engine.wait_next_applied_mc_block(&mc_handle, None)).await {
                    Ok(r_res) => break r_res?.0,
                    Err(tokio::time::error::Elapsed{..}) => {
                        log::warn!(target: "validator", "Validator manager didn't receive next applied master block after {}", mc_handle.id());
                    }
                }
            };
        }
    }
}

/// main entry point to validation process
pub fn start_validator_manager(engine: Arc<dyn EngineOperations>) {
    const CHECK_VALIDATOR_TIMEOUT: u64 = 60;    //secs
    tokio::spawn(async move {
        while !engine.get_validator_status() {
            log::trace!("is not a validator");
            tokio::time::sleep(Duration::from_secs(CHECK_VALIDATOR_TIMEOUT)).await;
        }
        log::info!("starting validator manager...");
        if let Err(e) = ValidatorManagerImpl::new(engine).invoke().await {
            log::error!(target: "validator", "FATAL!!! Unexpected error in validator manager: {:?}", e);
        }
    });
}

