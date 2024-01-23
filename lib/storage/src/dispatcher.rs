use std::num::NonZeroU32;
use std::ops::Deref;
use std::sync::Arc;
use std::time::{Duration, Instant};

use collection::config::ShardingMethod;
use common::defaults::CONSENSUS_META_OP_WAIT;

use crate::content_manager::collection_meta_ops::AliasOperations;
use crate::content_manager::shard_distribution::ShardDistributionProposal;
use crate::{
    ClusterStatus, CollectionMetaOperations, ConsensusOperations, ConsensusStateRef, StorageError,
    TableOfContent,
};

#[derive(Clone)]
pub struct Dispatcher {
    toc: Arc<TableOfContent>,
    consensus_state: Option<ConsensusStateRef>,
}

impl Dispatcher {
    pub fn new(toc: Arc<TableOfContent>) -> Self {
        Self {
            toc,
            consensus_state: None,
        }
    }

    pub fn with_consensus(self, state_ref: ConsensusStateRef) -> Self {
        Self {
            consensus_state: Some(state_ref),
            ..self
        }
    }

    pub fn toc(&self) -> &Arc<TableOfContent> {
        &self.toc
    }

    pub fn consensus_state(&self) -> Option<&ConsensusStateRef> {
        self.consensus_state.as_ref()
    }

    /// If `wait_timeout` is not supplied - then default duration will be used.
    /// This function needs to be called from a runtime with timers enabled.
    pub async fn submit_collection_meta_op(
        &self,
        operation: CollectionMetaOperations,
        wait_timeout: Option<Duration>,
    ) -> Result<bool, StorageError> {
        // if distributed deployment is enabled
        if let Some(state) = self.consensus_state.as_ref() {
            let start = Instant::now();

            let op = match operation {
                CollectionMetaOperations::CreateCollection(mut op) => {
                    self.toc.check_write_lock()?;
                    if !op.is_distribution_set() {
                        match op.create_collection.sharding_method.unwrap_or_default() {
                            ShardingMethod::Auto => {
                                // Suggest even distribution of shards across nodes
                                let number_of_peers = state.0.peer_count();
                                let shard_distribution = self
                                    .toc
                                    .suggest_shard_distribution(
                                        &op,
                                        NonZeroU32::new(number_of_peers as u32)
                                            .expect("Peer count should be always >= 1"),
                                    )
                                    .await;

                                op.set_distribution(shard_distribution);
                            }
                            ShardingMethod::Custom => {
                                // If custom sharding is used - we don't create any shards in advance
                                let empty_distribution = ShardDistributionProposal::empty();
                                op.set_distribution(empty_distribution);
                            }
                        }
                    }
                    CollectionMetaOperations::CreateCollection(op)
                }
                CollectionMetaOperations::CreateShardKey(op) => {
                    self.toc.check_write_lock()?;
                    CollectionMetaOperations::CreateShardKey(op)
                }

                op => op,
            };

            let do_sync_nodes = match &op {
                // Sync nodes after collection or shard key creation
                CollectionMetaOperations::CreateCollection(_)
                | CollectionMetaOperations::CreateShardKey(_) => true,
                // Sync nodes when creating or renaming collection aliases
                CollectionMetaOperations::ChangeAliases(changes) => {
                    changes.actions.iter().any(|change| match change {
                        AliasOperations::CreateAlias(_) | AliasOperations::RenameAlias(_) => true,
                        AliasOperations::DeleteAlias(_) => false,
                    })
                }
                // No need to sync nodes for other operations
                CollectionMetaOperations::UpdateCollection(_)
                | CollectionMetaOperations::DeleteCollection(_)
                | CollectionMetaOperations::TransferShard(_, _)
                | CollectionMetaOperations::SetShardReplicaState(_)
                | CollectionMetaOperations::DropShardKey(_)
                | CollectionMetaOperations::CreatePayloadIndex(_)
                | CollectionMetaOperations::DropPayloadIndex(_)
                | CollectionMetaOperations::Nop { .. } => false,
            };

            let res = state
                .propose_consensus_op_with_await(
                    ConsensusOperations::CollectionMeta(Box::new(op)),
                    wait_timeout,
                )
                .await?;


            // On some operations, synchronize all nodes to ensure all are ready for point operations
            if do_sync_nodes {
                let remaining_timeout =
                    wait_timeout.map(|timeout| timeout.saturating_sub(start.elapsed()));
                if let Err(err) = self.await_consensus_sync(remaining_timeout).await {
                    log::warn!("Failed to synchronize all nodes after collection operation in time, some nodes may not be ready: {err}");
                }
            }

            Ok(res)
        } else {
            if let CollectionMetaOperations::CreateCollection(_) = &operation {
                self.toc.check_write_lock()?;
            }
            self.toc.perform_collection_meta_op(operation).await
        }
    }

    pub fn cluster_status(&self) -> ClusterStatus {
        match self.consensus_state.as_ref() {
            Some(state) => state.cluster_status(),
            None => ClusterStatus::Disabled,
        }
    }

    pub async fn await_consensus_sync(
        &self,
        timeout: Option<Duration>,
    ) -> Result<(), StorageError> {
        let timeout = timeout.unwrap_or(CONSENSUS_META_OP_WAIT);

        if let Some(state) = self.consensus_state.as_ref() {
            let state = state.hard_state();
            let term = state.term;
            let commit = state.commit;
            let channel_service = self.toc.get_channel_service();
            let this_peer_id = self.toc.this_peer_id;

            channel_service
                .await_commit_on_all_peers(this_peer_id, commit, term, timeout)
                .await?;

            log::debug!(
                "Consensus is synchronized with term: {}, commit: {}",
                term,
                commit
            );

            Ok(())
        } else {
            Ok(())
        }
    }
}

impl Deref for Dispatcher {
    type Target = TableOfContent;

    fn deref(&self) -> &Self::Target {
        self.toc.deref()
    }
}
