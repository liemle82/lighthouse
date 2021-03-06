use super::{super::work_reprocessing_queue::ReprocessQueueMessage, Worker};
use crate::beacon_processor::worker::FUTURE_SLOT_TOLERANCE;
use crate::beacon_processor::{BlockResultSender, DuplicateCache};
use crate::metrics;
use crate::sync::manager::{SyncMessage, SyncRequestType};
use crate::sync::{BatchProcessResult, ChainId};
use beacon_chain::{
    BeaconChainError, BeaconChainTypes, BlockError, ChainSegmentResult, HistoricalBlockError,
};
use lighthouse_network::{PeerAction, PeerId};
use slog::{crit, debug, error, info, trace, warn};
use tokio::sync::mpsc;
use types::{Epoch, Hash256, SignedBeaconBlock};

/// Id associated to a block processing request, either a batch or a single block.
#[derive(Clone, Debug, PartialEq)]
pub enum ProcessId {
    /// Processing Id of a range syncing batch.
    RangeBatchId(ChainId, Epoch),
    /// Processing ID for a backfill syncing batch.
    BackSyncBatchId(Epoch),
    /// Processing Id of the parent lookup of a block.
    ParentLookup(PeerId, Hash256),
}

/// Returned when a chain segment import fails.
struct ChainSegmentFailed {
    /// To be displayed in logs.
    message: String,
    /// Used to penalize peers.
    peer_action: Option<PeerAction>,
}

impl<T: BeaconChainTypes> Worker<T> {
    /// Attempt to process a block received from a direct RPC request, returning the processing
    /// result on the `result_tx` channel.
    ///
    /// Raises a log if there are errors publishing the result to the channel.
    pub fn process_rpc_block(
        self,
        block: SignedBeaconBlock<T::EthSpec>,
        result_tx: BlockResultSender<T::EthSpec>,
        reprocess_tx: mpsc::Sender<ReprocessQueueMessage<T>>,
        duplicate_cache: DuplicateCache,
    ) {
        let block_root = block.canonical_root();
        // Checks if the block is already being imported through another source
        if let Some(handle) = duplicate_cache.check_and_insert(block_root) {
            let slot = block.slot();
            let block_result = self.chain.process_block(block);

            metrics::inc_counter(&metrics::BEACON_PROCESSOR_RPC_BLOCK_IMPORTED_TOTAL);

            if let Ok(root) = &block_result {
                info!(
                    self.log,
                    "New RPC block received";
                    "slot" => slot,
                    "hash" => %root
                );

                if reprocess_tx
                    .try_send(ReprocessQueueMessage::BlockImported(*root))
                    .is_err()
                {
                    error!(
                        self.log,
                        "Failed to inform block import";
                        "source" => "rpc",
                        "block_root" => %root,
                    )
                };
            }

            if result_tx.send(block_result).is_err() {
                crit!(self.log, "Failed return sync block result");
            }
            // Drop the handle to remove the entry from the cache
            drop(handle);
        } else {
            debug!(
                self.log,
                "Gossip block is being imported";
                "block_root" => %block_root,
            );
            // The gossip block that is being imported should eventually
            // trigger reprocessing of queued attestations once it is imported.
            // If the gossip block fails import, then it will be downscored
            // appropriately in `process_gossip_block`.

            // Here, we assume that the block will eventually be imported and
            // send a `BlockIsAlreadyKnown` message to sync.
            if result_tx
                .send(Err(BlockError::BlockIsAlreadyKnown))
                .is_err()
            {
                crit!(self.log, "Failed return sync block result");
            }
        }
    }

    /// Attempt to import the chain segment (`blocks`) to the beacon chain, informing the sync
    /// thread if more blocks are needed to process it.
    pub fn process_chain_segment(
        &self,
        process_id: ProcessId,
        downloaded_blocks: Vec<SignedBeaconBlock<T::EthSpec>>,
    ) {
        match process_id {
            // this a request from the range sync
            ProcessId::RangeBatchId(chain_id, epoch) => {
                let start_slot = downloaded_blocks.first().map(|b| b.slot().as_u64());
                let end_slot = downloaded_blocks.last().map(|b| b.slot().as_u64());
                let sent_blocks = downloaded_blocks.len();

                let result = match self.process_blocks(downloaded_blocks.iter()) {
                    (_, Ok(_)) => {
                        debug!(self.log, "Batch processed";
                            "batch_epoch" => epoch,
                            "first_block_slot" => start_slot,
                            "chain" => chain_id,
                            "last_block_slot" => end_slot,
                            "processed_blocks" => sent_blocks,
                            "service"=> "sync");
                        BatchProcessResult::Success(sent_blocks > 0)
                    }
                    (imported_blocks, Err(e)) => {
                        debug!(self.log, "Batch processing failed";
                            "batch_epoch" => epoch,
                            "first_block_slot" => start_slot,
                            "chain" => chain_id,
                            "last_block_slot" => end_slot,
                            "imported_blocks" => imported_blocks,
                            "error" => %e.message,
                            "service" => "sync");

                        BatchProcessResult::Failed {
                            imported_blocks: imported_blocks > 0,
                            peer_action: e.peer_action,
                        }
                    }
                };

                let sync_type = SyncRequestType::RangeSync(epoch, chain_id);

                self.send_sync_message(SyncMessage::BatchProcessed { sync_type, result });
            }
            // this a request from the Backfill sync
            ProcessId::BackSyncBatchId(epoch) => {
                let start_slot = downloaded_blocks.first().map(|b| b.slot().as_u64());
                let end_slot = downloaded_blocks.last().map(|b| b.slot().as_u64());
                let sent_blocks = downloaded_blocks.len();

                let result = match self.process_backfill_blocks(&downloaded_blocks) {
                    (_, Ok(_)) => {
                        debug!(self.log, "Backfill batch processed";
                            "batch_epoch" => epoch,
                            "first_block_slot" => start_slot,
                            "last_block_slot" => end_slot,
                            "processed_blocks" => sent_blocks,
                            "service"=> "sync");
                        BatchProcessResult::Success(sent_blocks > 0)
                    }
                    (_, Err(e)) => {
                        debug!(self.log, "Backfill batch processing failed";
                            "batch_epoch" => epoch,
                            "first_block_slot" => start_slot,
                            "last_block_slot" => end_slot,
                            "error" => %e.message,
                            "service" => "sync");
                        BatchProcessResult::Failed {
                            imported_blocks: false,
                            peer_action: e.peer_action,
                        }
                    }
                };

                let sync_type = SyncRequestType::BackFillSync(epoch);

                self.send_sync_message(SyncMessage::BatchProcessed { sync_type, result });
            }
            // this is a parent lookup request from the sync manager
            ProcessId::ParentLookup(peer_id, chain_head) => {
                debug!(
                    self.log, "Processing parent lookup";
                    "last_peer_id" => %peer_id,
                    "blocks" => downloaded_blocks.len()
                );
                // parent blocks are ordered from highest slot to lowest, so we need to process in
                // reverse
                match self.process_blocks(downloaded_blocks.iter().rev()) {
                    (_, Err(e)) => {
                        debug!(self.log, "Parent lookup failed"; "last_peer_id" => %peer_id, "error" => %e.message);
                        self.send_sync_message(SyncMessage::ParentLookupFailed {
                            peer_id,
                            chain_head,
                        })
                    }
                    (_, Ok(_)) => {
                        debug!(self.log, "Parent lookup processed successfully");
                    }
                }
            }
        }
    }

    /// Helper function to process blocks batches which only consumes the chain and blocks to process.
    fn process_blocks<'a>(
        &self,
        downloaded_blocks: impl Iterator<Item = &'a SignedBeaconBlock<T::EthSpec>>,
    ) -> (usize, Result<(), ChainSegmentFailed>) {
        let blocks = downloaded_blocks.cloned().collect::<Vec<_>>();
        match self.chain.process_chain_segment(blocks) {
            ChainSegmentResult::Successful { imported_blocks } => {
                metrics::inc_counter(&metrics::BEACON_PROCESSOR_CHAIN_SEGMENT_SUCCESS_TOTAL);
                if imported_blocks > 0 {
                    // Batch completed successfully with at least one block, run fork choice.
                    self.run_fork_choice();
                }

                (imported_blocks, Ok(()))
            }
            ChainSegmentResult::Failed {
                imported_blocks,
                error,
            } => {
                metrics::inc_counter(&metrics::BEACON_PROCESSOR_CHAIN_SEGMENT_FAILED_TOTAL);
                let r = self.handle_failed_chain_segment(error);
                if imported_blocks > 0 {
                    self.run_fork_choice();
                }
                (imported_blocks, r)
            }
        }
    }

    /// Helper function to process backfill block batches which only consumes the chain and blocks to process.
    fn process_backfill_blocks(
        &self,
        blocks: &[SignedBeaconBlock<T::EthSpec>],
    ) -> (usize, Result<(), ChainSegmentFailed>) {
        match self.chain.import_historical_block_batch(blocks) {
            Ok(imported_blocks) => {
                metrics::inc_counter(
                    &metrics::BEACON_PROCESSOR_BACKFILL_CHAIN_SEGMENT_SUCCESS_TOTAL,
                );

                (imported_blocks, Ok(()))
            }
            Err(error) => {
                metrics::inc_counter(
                    &metrics::BEACON_PROCESSOR_BACKFILL_CHAIN_SEGMENT_FAILED_TOTAL,
                );
                let err = match error {
                    // Handle the historical block errors specifically
                    BeaconChainError::HistoricalBlockError(e) => match e {
                        HistoricalBlockError::MismatchedBlockRoot {
                            block_root,
                            expected_block_root,
                        } => {
                            debug!(
                                self.log,
                                "Backfill batch processing error";
                                "error" => "mismatched_block_root",
                                "block_root" => ?block_root,
                                "expected_root" => ?expected_block_root
                            );

                            ChainSegmentFailed {
                                message: String::from("mismatched_block_root"),
                                // The peer is faulty if they send blocks with bad roots.
                                peer_action: Some(PeerAction::LowToleranceError),
                            }
                        }
                        HistoricalBlockError::InvalidSignature
                        | HistoricalBlockError::SignatureSet(_) => {
                            warn!(
                                self.log,
                                "Backfill batch processing error";
                                "error" => ?e
                            );

                            ChainSegmentFailed {
                                message: "invalid_signature".into(),
                                // The peer is faulty if they bad signatures.
                                peer_action: Some(PeerAction::LowToleranceError),
                            }
                        }
                        HistoricalBlockError::ValidatorPubkeyCacheTimeout => {
                            warn!(
                                self.log,
                                "Backfill batch processing error";
                                "error" => "pubkey_cache_timeout"
                            );

                            ChainSegmentFailed {
                                message: "pubkey_cache_timeout".into(),
                                // This is an internal error, do not penalize the peer.
                                peer_action: None,
                            }
                        }
                        HistoricalBlockError::NoAnchorInfo => {
                            warn!(self.log, "Backfill not required");

                            ChainSegmentFailed {
                                message: String::from("no_anchor_info"),
                                // There is no need to do a historical sync, this is not a fault of
                                // the peer.
                                peer_action: None,
                            }
                        }
                        HistoricalBlockError::IndexOutOfBounds => {
                            error!(
                                self.log,
                                "Backfill batch OOB error";
                                "error" => ?e,
                            );
                            ChainSegmentFailed {
                                message: String::from("logic_error"),
                                // This should never occur, don't penalize the peer.
                                peer_action: None,
                            }
                        }
                        HistoricalBlockError::BlockOutOfRange { .. } => {
                            error!(
                                self.log,
                                "Backfill batch error";
                                "error" => ?e,
                            );
                            ChainSegmentFailed {
                                message: String::from("unexpected_error"),
                                // This should never occur, don't penalize the peer.
                                peer_action: None,
                            }
                        }
                    },
                    other => {
                        warn!(self.log, "Backfill batch processing error"; "error" => ?other);
                        ChainSegmentFailed {
                            message: format!("{:?}", other),
                            // This is an internal error, don't penalize the peer.
                            peer_action: None,
                        }
                    }
                };
                (0, Err(err))
            }
        }
    }

    /// Runs fork-choice on a given chain. This is used during block processing after one successful
    /// block import.
    fn run_fork_choice(&self) {
        match self.chain.fork_choice() {
            Ok(()) => trace!(
                self.log,
                "Fork choice success";
                "location" => "batch processing"
            ),
            Err(e) => error!(
                self.log,
                "Fork choice failed";
                "error" => ?e,
                "location" => "batch import error"
            ),
        }
    }

    /// Helper function to handle a `BlockError` from `process_chain_segment`
    fn handle_failed_chain_segment(
        &self,
        error: BlockError<T::EthSpec>,
    ) -> Result<(), ChainSegmentFailed> {
        match error {
            BlockError::ParentUnknown(block) => {
                // blocks should be sequential and all parents should exist
                Err(ChainSegmentFailed {
                    message: format!("Block has an unknown parent: {}", block.parent_root()),
                    // Peers are faulty if they send non-sequential blocks.
                    peer_action: Some(PeerAction::LowToleranceError),
                })
            }
            BlockError::BlockIsAlreadyKnown => {
                // This can happen for many reasons. Head sync's can download multiples and parent
                // lookups can download blocks before range sync
                Ok(())
            }
            BlockError::FutureSlot {
                present_slot,
                block_slot,
            } => {
                if present_slot + FUTURE_SLOT_TOLERANCE >= block_slot {
                    // The block is too far in the future, drop it.
                    warn!(
                        self.log, "Block is ahead of our slot clock";
                        "msg" => "block for future slot rejected, check your time",
                        "present_slot" => present_slot,
                        "block_slot" => block_slot,
                        "FUTURE_SLOT_TOLERANCE" => FUTURE_SLOT_TOLERANCE,
                    );
                } else {
                    // The block is in the future, but not too far.
                    debug!(
                        self.log, "Block is slightly ahead of our slot clock, ignoring.";
                        "present_slot" => present_slot,
                        "block_slot" => block_slot,
                        "FUTURE_SLOT_TOLERANCE" => FUTURE_SLOT_TOLERANCE,
                    );
                }

                Err(ChainSegmentFailed {
                    message: format!(
                        "Block with slot {} is higher than the current slot {}",
                        block_slot, present_slot
                    ),
                    // Peers are faulty if they send blocks from the future.
                    peer_action: Some(PeerAction::LowToleranceError),
                })
            }
            BlockError::WouldRevertFinalizedSlot { .. } => {
                debug!(self.log, "Finalized or earlier block processed";);
                Ok(())
            }
            BlockError::GenesisBlock => {
                debug!(self.log, "Genesis block was processed");
                Ok(())
            }
            BlockError::BeaconChainError(e) => {
                warn!(
                    self.log, "BlockProcessingFailure";
                    "msg" => "unexpected condition in processing block.",
                    "outcome" => ?e,
                );

                Err(ChainSegmentFailed {
                    message: format!("Internal error whilst processing block: {:?}", e),
                    // Do not penalize peers for internal errors.
                    peer_action: None,
                })
            }
            other => {
                debug!(
                    self.log, "Invalid block received";
                    "msg" => "peer sent invalid block",
                    "outcome" => %other,
                );

                Err(ChainSegmentFailed {
                    message: format!("Peer sent invalid block. Reason: {:?}", other),
                    // Do not penalize peers for internal errors.
                    peer_action: None,
                })
            }
        }
    }
}
