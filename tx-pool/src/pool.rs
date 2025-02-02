//! Top-level Pool type, methods, and tests
extern crate rustc_hash;
extern crate slab;
use super::component::{commit_txs_scanner::CommitTxsScanner, TxEntry};
use crate::callback::Callbacks;
use crate::component::pool_map::{PoolEntry, PoolMap, Status};
use crate::component::recent_reject::RecentReject;
use crate::error::Reject;
use crate::pool_cell::PoolCell;
use ckb_app_config::TxPoolConfig;
use ckb_logger::{debug, error, warn};
use ckb_snapshot::Snapshot;
use ckb_store::ChainStore;
use ckb_types::core::CapacityError;
use ckb_types::{
    core::{
        cell::{resolve_transaction, OverlayCellChecker, OverlayCellProvider, ResolvedTransaction},
        tx_pool::{TxPoolEntryInfo, TxPoolIds},
        Capacity, Cycle, TransactionView, UncleBlockView,
    },
    packed::{Byte32, ProposalShortId},
};
use lru::LruCache;
use std::collections::HashSet;
use std::sync::Arc;

const COMMITTED_HASH_CACHE_SIZE: usize = 100_000;
const MAX_REPLACEMENT_CANDIDATES: usize = 100;

/// Tx-pool implementation
pub struct TxPool {
    pub(crate) config: TxPoolConfig,
    pub(crate) pool_map: PoolMap,
    /// cache for committed transactions hash
    pub(crate) committed_txs_hash_cache: LruCache<ProposalShortId, Byte32>,
    // sum of all tx_pool tx's virtual sizes.
    pub(crate) total_tx_size: usize,
    // sum of all tx_pool tx's cycles.
    pub(crate) total_tx_cycles: Cycle,
    /// storage snapshot reference
    pub(crate) snapshot: Arc<Snapshot>,
    /// record recent reject
    pub recent_reject: Option<RecentReject>,
    // expiration milliseconds,
    pub(crate) expiry: u64,
}

impl TxPool {
    /// Create new TxPool
    pub fn new(config: TxPoolConfig, snapshot: Arc<Snapshot>) -> TxPool {
        let recent_reject = Self::build_recent_reject(&config);
        let expiry = config.expiry_hours as u64 * 60 * 60 * 1000;
        TxPool {
            pool_map: PoolMap::new(config.max_ancestors_count),
            committed_txs_hash_cache: LruCache::new(COMMITTED_HASH_CACHE_SIZE),
            total_tx_size: 0,
            total_tx_cycles: 0,
            config,
            snapshot,
            recent_reject,
            expiry,
        }
    }

    /// Tx-pool owned snapshot, it may not consistent with chain cause tx-pool update snapshot asynchronously
    pub(crate) fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// Makes a clone of the `Arc<Snapshot>`
    pub(crate) fn cloned_snapshot(&self) -> Arc<Snapshot> {
        Arc::clone(&self.snapshot)
    }

    fn get_by_status(&self, status: Status) -> Vec<&PoolEntry> {
        self.pool_map.get_by_status(status)
    }

    /// Get tx-pool size
    pub fn status_size(&self, status: Status) -> usize {
        self.get_by_status(status).len()
    }

    /// Update size and cycles statics for add tx
    pub fn update_statics_for_add_tx(&mut self, tx_size: usize, cycles: Cycle) {
        self.total_tx_size += tx_size;
        self.total_tx_cycles += cycles;
    }

    /// Check whether tx-pool enable RBF
    pub fn enable_rbf(&self) -> bool {
        self.config.min_rbf_rate > self.config.min_fee_rate
    }

    /// The least required fee rate to allow tx to be replaced
    pub fn min_replace_fee(&self, tx: &TxEntry) -> Option<Capacity> {
        if !self.enable_rbf() {
            return None;
        }
        let entry = vec![self.get_pool_entry(&tx.proposal_short_id()).unwrap()];
        self.calculate_min_replace_fee(&entry, tx.size)
    }

    /// min_replace_fee = sum(replaced_txs.fee) + extra_rbf_fee
    fn calculate_min_replace_fee(&self, conflicts: &[&PoolEntry], size: usize) -> Option<Capacity> {
        let extra_rbf_fee = self.config.min_rbf_rate.fee(size as u64);
        let replaced_sum_fee = conflicts
            .iter()
            .map(|c| c.inner.fee)
            .try_fold(Capacity::zero(), |acc, x| acc.safe_add(x));
        let res = replaced_sum_fee.map_or(Err(CapacityError::Overflow), |sum| {
            sum.safe_add(extra_rbf_fee)
        });
        if let Ok(res) = res {
            Some(res)
        } else {
            let fees = conflicts.iter().map(|c| c.inner.fee).collect::<Vec<_>>();
            error!(
                "conflicts: {:?} replaced_sum_fee {:?} overflow by add {}",
                conflicts.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
                fees,
                extra_rbf_fee
            );
            None
        }
    }

    /// Update size and cycles statics for remove tx
    /// cycles overflow is possible, currently obtaining cycles is not accurate
    pub fn update_statics_for_remove_tx(&mut self, tx_size: usize, cycles: Cycle) {
        let total_tx_size = self.total_tx_size.checked_sub(tx_size).unwrap_or_else(|| {
            error!(
                "total_tx_size {} overflow by sub {}",
                self.total_tx_size, tx_size
            );
            0
        });
        let total_tx_cycles = self.total_tx_cycles.checked_sub(cycles).unwrap_or_else(|| {
            error!(
                "total_tx_cycles {} overflow by sub {}",
                self.total_tx_cycles, cycles
            );
            0
        });
        self.total_tx_size = total_tx_size;
        self.total_tx_cycles = total_tx_cycles;
    }

    /// Add tx with pending status
    /// If did have this value present, false is returned.
    pub(crate) fn add_pending(&mut self, entry: TxEntry) -> Result<bool, Reject> {
        self.pool_map.add_entry(entry, Status::Pending)
    }

    /// Add tx which proposed but still uncommittable to gap
    pub(crate) fn add_gap(&mut self, entry: TxEntry) -> Result<bool, Reject> {
        self.pool_map.add_entry(entry, Status::Gap)
    }

    /// Add tx with proposed status
    pub(crate) fn add_proposed(&mut self, entry: TxEntry) -> Result<bool, Reject> {
        self.pool_map.add_entry(entry, Status::Proposed)
    }

    /// Returns true if the tx-pool contains a tx with specified id.
    pub(crate) fn contains_proposal_id(&self, id: &ProposalShortId) -> bool {
        self.pool_map.get_by_id(id).is_some()
    }

    pub(crate) fn set_entry_proposed(&mut self, short_id: &ProposalShortId) {
        self.pool_map.set_entry(short_id, Status::Proposed)
    }

    pub(crate) fn set_entry_gap(&mut self, short_id: &ProposalShortId) {
        self.pool_map.set_entry(short_id, Status::Gap)
    }

    /// Returns tx with cycles corresponding to the id.
    pub(crate) fn get_tx_with_cycles(
        &self,
        id: &ProposalShortId,
    ) -> Option<(TransactionView, Cycle)> {
        self.pool_map
            .get_by_id(id)
            .map(|entry| (entry.inner.transaction().clone(), entry.inner.cycles))
    }

    pub(crate) fn get_pool_entry(&self, id: &ProposalShortId) -> Option<&PoolEntry> {
        self.pool_map.get_by_id(id)
    }

    pub(crate) fn get_tx_from_pool(&self, id: &ProposalShortId) -> Option<&TransactionView> {
        self.pool_map
            .get_by_id(id)
            .map(|entry| entry.inner.transaction())
    }

    pub(crate) fn remove_committed_txs<'a>(
        &mut self,
        txs: impl Iterator<Item = &'a TransactionView>,
        callbacks: &Callbacks,
        detached_headers: &HashSet<Byte32>,
    ) {
        for tx in txs {
            let tx_hash = tx.hash();
            debug!("try remove_committed_tx {}", tx_hash);
            self.remove_committed_tx(tx, callbacks);

            self.committed_txs_hash_cache
                .put(tx.proposal_short_id(), tx_hash);
        }

        if !detached_headers.is_empty() {
            self.resolve_conflict_header_dep(detached_headers, callbacks)
        }
    }

    fn resolve_conflict_header_dep(
        &mut self,
        detached_headers: &HashSet<Byte32>,
        callbacks: &Callbacks,
    ) {
        for (entry, reject) in self.pool_map.resolve_conflict_header_dep(detached_headers) {
            callbacks.call_reject(self, &entry, reject);
        }
    }

    fn remove_committed_tx(&mut self, tx: &TransactionView, callbacks: &Callbacks) {
        let short_id = tx.proposal_short_id();
        if let Some(entry) = self.pool_map.remove_entry(&short_id) {
            debug!("remove_committed_tx for {}", tx.hash());
            callbacks.call_committed(self, &entry)
        }
        {
            let conflicts = self.pool_map.resolve_conflict(tx);
            for (entry, reject) in conflicts {
                callbacks.call_reject(self, &entry, reject);
            }
        }
    }

    // Expire all transaction (and their dependencies) in the pool.
    pub(crate) fn remove_expired(&mut self, callbacks: &Callbacks) {
        let now_ms = ckb_systemtime::unix_time_as_millis();
        let removed: Vec<_> = self
            .pool_map
            .iter()
            .filter(|&entry| self.expiry + entry.inner.timestamp < now_ms)
            .map(|entry| entry.inner.clone())
            .collect();

        for entry in removed {
            let tx_hash = entry.transaction().hash();
            debug!("remove_expired {} timestamp({})", tx_hash, entry.timestamp);
            self.pool_map.remove_entry(&entry.proposal_short_id());
            let reject = Reject::Expiry(entry.timestamp);
            callbacks.call_reject(self, &entry, reject);
        }
    }

    // Remove transactions from the pool until total size <= size_limit.
    pub(crate) fn limit_size(&mut self, callbacks: &Callbacks) {
        while self.total_tx_size > self.config.max_tx_pool_size {
            let next_evict_entry = || {
                self.pool_map
                    .next_evict_entry(Status::Pending)
                    .or_else(|| self.pool_map.next_evict_entry(Status::Gap))
                    .or_else(|| self.pool_map.next_evict_entry(Status::Proposed))
            };

            if let Some(id) = next_evict_entry() {
                let removed = self.pool_map.remove_entry_and_descendants(&id);
                for entry in removed {
                    let tx_hash = entry.transaction().hash();
                    debug!(
                        "removed by size limit {} timestamp({})",
                        tx_hash, entry.timestamp
                    );
                    let reject = Reject::Full(format!(
                        "the fee_rate for this transaction is: {}",
                        entry.fee_rate()
                    ));
                    callbacks.call_reject(self, &entry, reject);
                }
            }
        }
        self.pool_map.entries.shrink_to_fit();
    }

    // remove transaction with detached proposal from gap and proposed
    // try re-put to pending
    pub(crate) fn remove_by_detached_proposal<'a>(
        &mut self,
        ids: impl Iterator<Item = &'a ProposalShortId>,
    ) {
        for id in ids {
            if let Some(e) = self.pool_map.get_by_id(id) {
                let status = e.status;
                if status == Status::Pending {
                    continue;
                }
                let mut entries = self.pool_map.remove_entry_and_descendants(id);
                entries.sort_unstable_by_key(|entry| entry.ancestors_count);
                for mut entry in entries {
                    let tx_hash = entry.transaction().hash();
                    entry.reset_statistic_state();
                    let ret = self.add_pending(entry);
                    debug!(
                        "remove_by_detached_proposal from {:?} {} add_pending {:?}",
                        status, tx_hash, ret
                    );
                }
            }
        }
    }

    pub(crate) fn remove_tx(&mut self, id: &ProposalShortId) -> bool {
        let entries = self.pool_map.remove_entry_and_descendants(id);
        if !entries.is_empty() {
            for entry in entries {
                self.update_statics_for_remove_tx(entry.size, entry.cycles);
            }
            return true;
        }

        if let Some(entry) = self.pool_map.remove_entry(id) {
            self.update_statics_for_remove_tx(entry.size, entry.cycles);
            return true;
        }
        false
    }

    pub(crate) fn check_rtx_from_pool(&self, rtx: &ResolvedTransaction) -> Result<(), Reject> {
        let snapshot = self.snapshot();
        let pool_cell = PoolCell::new(&self.pool_map, false);
        let checker = OverlayCellChecker::new(&pool_cell, snapshot);
        let mut seen_inputs = HashSet::new();
        rtx.check(&mut seen_inputs, &checker, snapshot)
            .map_err(Reject::Resolve)
    }

    pub(crate) fn resolve_tx_from_pool(
        &self,
        tx: TransactionView,
        rbf: bool,
    ) -> Result<Arc<ResolvedTransaction>, Reject> {
        let snapshot = self.snapshot();
        let pool_cell = PoolCell::new(&self.pool_map, rbf);
        let provider = OverlayCellProvider::new(&pool_cell, snapshot);
        let mut seen_inputs = HashSet::new();
        resolve_transaction(tx, &mut seen_inputs, &provider, snapshot)
            .map(Arc::new)
            .map_err(Reject::Resolve)
    }

    pub(crate) fn gap_rtx(&mut self, short_id: &ProposalShortId) -> Result<(), Reject> {
        match self.get_pool_entry(short_id) {
            Some(entry) => {
                let tx_hash = entry.inner.transaction().hash();
                if entry.status == Status::Gap {
                    Err(Reject::Duplicated(tx_hash))
                } else {
                    debug!("gap_rtx: {:?} => {:?}", tx_hash, short_id);
                    self.set_entry_gap(short_id);
                    Ok(())
                }
            }
            None => Err(Reject::Malformed(
                String::from("invalid short_id"),
                Default::default(),
            )),
        }
    }

    pub(crate) fn proposed_rtx(&mut self, short_id: &ProposalShortId) -> Result<(), Reject> {
        match self.get_pool_entry(short_id) {
            Some(entry) => {
                let tx_hash = entry.inner.transaction().hash();
                if entry.status == Status::Proposed {
                    Err(Reject::Duplicated(tx_hash))
                } else {
                    debug!("proposed_rtx: {:?} => {:?}", tx_hash, short_id);
                    self.set_entry_proposed(short_id);
                    Ok(())
                }
            }
            None => Err(Reject::Malformed(
                String::from("invalid short_id"),
                Default::default(),
            )),
        }
    }

    /// Get to-be-proposal transactions that may be included in the next block.
    pub(crate) fn get_proposals(
        &self,
        limit: usize,
        exclusion: &HashSet<ProposalShortId>,
    ) -> HashSet<ProposalShortId> {
        let mut proposals = HashSet::with_capacity(limit);
        self.pool_map
            .fill_proposals(limit, exclusion, &mut proposals, Status::Pending);
        proposals
    }

    /// Returns tx from tx-pool or storage corresponding to the id.
    pub(crate) fn get_tx_from_pool_or_store(
        &self,
        proposal_id: &ProposalShortId,
    ) -> Option<TransactionView> {
        self.get_tx_from_pool(proposal_id).cloned().or_else(|| {
            self.committed_txs_hash_cache
                .peek(proposal_id)
                .and_then(|tx_hash| self.snapshot().get_transaction(tx_hash).map(|(tx, _)| tx))
        })
    }

    pub(crate) fn get_ids(&self) -> TxPoolIds {
        let pending = self
            .pool_map
            .score_sorted_iter_by(vec![Status::Pending, Status::Gap])
            .map(|entry| entry.transaction().hash())
            .collect();

        let proposed = self
            .pool_map
            .sorted_proposed_iter()
            .map(|entry| entry.transaction().hash())
            .collect();

        TxPoolIds { pending, proposed }
    }

    pub(crate) fn get_all_entry_info(&self) -> TxPoolEntryInfo {
        let pending = self
            .pool_map
            .score_sorted_iter_by(vec![Status::Pending, Status::Gap])
            .map(|entry| (entry.transaction().hash(), entry.to_info()))
            .collect();

        let proposed = self
            .pool_map
            .sorted_proposed_iter()
            .map(|entry| (entry.transaction().hash(), entry.to_info()))
            .collect();

        TxPoolEntryInfo { pending, proposed }
    }

    pub(crate) fn drain_all_transactions(&mut self) -> Vec<TransactionView> {
        let mut txs = CommitTxsScanner::new(&self.pool_map)
            .txs_to_commit(self.total_tx_size, self.total_tx_cycles)
            .0
            .into_iter()
            .map(|tx_entry| tx_entry.into_transaction())
            .collect::<Vec<_>>();
        let mut pending = self
            .pool_map
            .entries
            .remove_by_status(&Status::Pending)
            .into_iter()
            .map(|e| e.inner.into_transaction())
            .collect::<Vec<_>>();
        txs.append(&mut pending);
        let mut gap = self
            .pool_map
            .entries
            .remove_by_status(&Status::Gap)
            .into_iter()
            .map(|e| e.inner.into_transaction())
            .collect::<Vec<_>>();
        txs.append(&mut gap);
        self.total_tx_size = 0;
        self.total_tx_cycles = 0;
        self.pool_map.clear();
        txs
    }

    pub(crate) fn clear(&mut self, snapshot: Arc<Snapshot>) {
        self.pool_map.clear();
        self.snapshot = snapshot;
        self.committed_txs_hash_cache = LruCache::new(COMMITTED_HASH_CACHE_SIZE);
        self.total_tx_size = 0;
        self.total_tx_cycles = 0;
    }

    pub(crate) fn package_proposals(
        &self,
        proposals_limit: u64,
        uncles: &[UncleBlockView],
    ) -> HashSet<ProposalShortId> {
        let uncle_proposals = uncles
            .iter()
            .flat_map(|u| u.data().proposals().into_iter())
            .collect();
        self.get_proposals(proposals_limit as usize, &uncle_proposals)
    }

    pub(crate) fn package_txs(
        &self,
        max_block_cycles: Cycle,
        txs_size_limit: usize,
    ) -> (Vec<TxEntry>, usize, Cycle) {
        let (entries, size, cycles) =
            CommitTxsScanner::new(&self.pool_map).txs_to_commit(txs_size_limit, max_block_cycles);

        if !entries.is_empty() {
            ckb_logger::info!(
                "[get_block_template] candidate txs count: {}, size: {}/{}, cycles:{}/{}",
                entries.len(),
                size,
                txs_size_limit,
                cycles,
                max_block_cycles
            );
        }
        (entries, size, cycles)
    }

    pub(crate) fn check_rbf(
        &self,
        snapshot: &Snapshot,
        rtx: &ResolvedTransaction,
        conflict_ids: &HashSet<ProposalShortId>,
        fee: Capacity,
        tx_size: usize,
    ) -> Result<(), Reject> {
        assert!(self.enable_rbf());
        assert!(!conflict_ids.is_empty());

        let conflicts = conflict_ids
            .iter()
            .filter_map(|id| self.get_pool_entry(id))
            .collect::<Vec<_>>();
        assert!(conflicts.len() == conflict_ids.len());

        let short_id = rtx.transaction.proposal_short_id();
        // Rule #4, new tx's fee need to higher than min_rbf_fee computed from the tx_pool configuration
        // Rule #3, new tx's fee need to higher than conflicts, here we only check the root tx
        if let Some(min_replace_fee) = self.calculate_min_replace_fee(&conflicts, tx_size) {
            if fee < min_replace_fee {
                return Err(Reject::RBFRejected(format!(
                    "Tx's current fee is {}, expect it to >= {} to replace old txs",
                    fee, min_replace_fee,
                )));
            }
        } else {
            return Err(Reject::RBFRejected(
                "calculate_min_replace_fee failed".to_string(),
            ));
        }

        // Rule #2, new tx don't contain any new unconfirmed inputs
        let mut inputs = HashSet::new();
        let mut outputs = HashSet::new();
        for c in conflicts.iter() {
            inputs.extend(c.inner.transaction().input_pts_iter());
            outputs.extend(c.inner.transaction().output_pts_iter());
        }

        if rtx
            .transaction
            .input_pts_iter()
            .any(|pt| !inputs.contains(&pt) && !snapshot.transaction_exists(&pt.tx_hash()))
        {
            return Err(Reject::RBFRejected(
                "new Tx contains unconfirmed inputs".to_string(),
            ));
        }

        if rtx
            .transaction
            .cell_deps_iter()
            .any(|dep| outputs.contains(&dep.out_point()))
        {
            return Err(Reject::RBFRejected(
                "new Tx contains cell deps from conflicts".to_string(),
            ));
        }

        // Rule #5, the replaced tx's descendants can not more than 100
        // and the ancestor of the new tx don't have common set with the replaced tx's descendants
        let mut replace_count: usize = 0;
        let ancestors = self.pool_map.calc_ancestors(&short_id);
        for conflict in conflicts.iter() {
            let descendants = self.pool_map.calc_descendants(&conflict.id);
            replace_count += descendants.len() + 1;
            if replace_count > MAX_REPLACEMENT_CANDIDATES {
                return Err(Reject::RBFRejected(format!(
                    "Tx conflict too many txs, conflict txs count: {}",
                    replace_count,
                )));
            }

            if !descendants.is_disjoint(&ancestors) {
                return Err(Reject::RBFRejected(
                    "Tx ancestors have common with conflict Tx descendants".to_string(),
                ));
            }

            let entries = descendants
                .iter()
                .filter_map(|id| self.get_pool_entry(id))
                .collect::<Vec<_>>();

            for entry in entries.iter() {
                let hash = entry.inner.transaction().hash();
                if rtx
                    .transaction
                    .input_pts_iter()
                    .any(|pt| pt.tx_hash() == hash)
                {
                    return Err(Reject::RBFRejected(
                        "new Tx contains inputs in descendants of to be replaced Tx".to_string(),
                    ));
                }
            }

            let mut entries_status = entries.iter().map(|e| e.status).collect::<Vec<_>>();
            entries_status.push(conflict.status);
            // Rule #6, all conflict Txs should be in `Pending` or `Gap` status
            if entries_status
                .iter()
                .any(|s| ![Status::Pending, Status::Gap].contains(s))
            {
                // Here we only refer to `Pending` status, since `Gap` is an internal status
                return Err(Reject::RBFRejected(
                    "all conflict Txs should be in Pending status".to_string(),
                ));
            }
        }

        Ok(())
    }

    fn build_recent_reject(config: &TxPoolConfig) -> Option<RecentReject> {
        if !config.recent_reject.as_os_str().is_empty() {
            let recent_reject_ttl =
                u8::max(1, config.keep_rejected_tx_hashes_days) as i32 * 24 * 60 * 60;
            match RecentReject::new(
                &config.recent_reject,
                config.keep_rejected_tx_hashes_count,
                recent_reject_ttl,
            ) {
                Ok(recent_reject) => Some(recent_reject),
                Err(err) => {
                    error!(
                        "Failed to open recent reject database {:?} {}",
                        config.recent_reject, err
                    );
                    None
                }
            }
        } else {
            warn!("Recent reject database is disabled!");
            None
        }
    }
}
