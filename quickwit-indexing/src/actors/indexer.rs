// Copyright (C) 2022 Quickwit, Inc.
//
// Quickwit is offered under the AGPL v3.0 and as commercial software.
// For commercial licensing, contact us at hello@quickwit.io.
//
// AGPL:
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

use std::collections::hash_map::Entry;
use std::ops::RangeInclusive;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use async_trait::async_trait;
use fail::fail_point;
use fnv::FnvHashMap;
use itertools::Itertools;
use quickwit_actors::{Actor, ActorContext, ActorExitStatus, Handler, Mailbox, QueueCapacity};
use quickwit_common::runtimes::RuntimeType;
use quickwit_config::IndexingSettings;
use quickwit_doc_mapper::{DocMapper, DocParsingError, SortBy, QUICKWIT_TOKENIZER_MANAGER};
use quickwit_metastore::checkpoint::{IndexCheckpointDelta, SourceCheckpointDelta};
use quickwit_metastore::Metastore;
use tantivy::schema::{Field, Schema, Value};
use tantivy::store::{Compressor, ZstdCompressor};
use tantivy::{Document, IndexBuilder, IndexSettings, IndexSortByField};
use tokio::runtime::Handle;
use tracing::{info, warn};
use ulid::Ulid;

use crate::actors::Packager;
use crate::models::{
    IndexedSplit, IndexedSplitBatch, IndexingDirectory, IndexingPipelineId, NewPublishLock,
    PublishLock, RawDocBatch,
};

#[derive(Debug)]
struct CommitTimeout {
    workbench_id: Ulid,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IndexerCounters {
    /// Overall number of documents received, partitioned
    /// into 3 categories:
    /// - number docs that did not parse correctly.
    /// - number docs missing a timestamp (if the index has no timestamp,
    /// then this counter is 0)
    /// - number of valid docs.
    pub num_parse_errors: u64,
    pub num_missing_fields: u64,
    pub num_valid_docs: u64,

    /// Number of splits that were emitted by the indexer.
    pub num_splits_emitted: u64,

    /// Number of split batches that were emitted by the indexer.
    pub num_split_batches_emitted: u64,

    /// Number of bytes that went through the indexer
    /// during its entire lifetime.
    ///
    /// Includes both valid and invalid documents.
    pub overall_num_bytes: u64,

    /// Number of (valid) documents in the current workbench.
    /// This value is used to trigger commit and for observation.
    pub num_docs_in_workbench: u64,
}

impl IndexerCounters {
    /// Returns the overall number of docs that went through the indexer (valid or not).
    pub fn num_processed_docs(&self) -> u64 {
        self.num_valid_docs + self.num_parse_errors + self.num_missing_fields
    }

    /// Returns the overall number of docs that were sent to the indexer but were invalid.
    /// (For instance, because they were missing a required field or because their because
    /// their format was invalid)
    pub fn num_invalid_docs(&self) -> u64 {
        self.num_parse_errors + self.num_missing_fields
    }
}

struct IndexerState {
    pipeline_id: IndexingPipelineId,
    doc_mapper: Arc<dyn DocMapper>,
    indexing_directory: IndexingDirectory,
    indexing_settings: IndexingSettings,
    publish_lock: PublishLock,
    timestamp_field_opt: Option<Field>,
    schema: Schema,
    index_settings: IndexSettings,
}

enum PrepareDocumentOutcome {
    ParsingError,
    MissingField,
    Document {
        document: Document,
        timestamp_opt: Option<i64>,
        partition: u64,
    },
}

impl IndexerState {
    fn create_indexed_split(
        &self,
        partition_id: u64,
        ctx: &ActorContext<Indexer>,
    ) -> anyhow::Result<IndexedSplit> {
        let index_builder = IndexBuilder::new()
            .settings(self.index_settings.clone())
            .schema(self.schema.clone())
            .tokenizers(QUICKWIT_TOKENIZER_MANAGER.clone());
        let indexed_split = IndexedSplit::new_in_dir(
            self.pipeline_id.clone(),
            partition_id,
            self.indexing_directory.scratch_directory.clone(),
            self.indexing_settings.resources.clone(),
            index_builder,
            ctx.progress().clone(),
            ctx.kill_switch().clone(),
        )?;
        info!(split_id = indexed_split.split_id(), "new-split");
        Ok(indexed_split)
    }

    fn get_or_create_indexed_split<'a>(
        &self,
        partition_id: u64,
        splits: &'a mut FnvHashMap<u64, IndexedSplit>,
        ctx: &ActorContext<Indexer>,
    ) -> anyhow::Result<&'a mut IndexedSplit> {
        match splits.entry(partition_id) {
            Entry::Occupied(indexed_split) => Ok(indexed_split.into_mut()),
            Entry::Vacant(vacant_entry) => {
                let indexed_split = self.create_indexed_split(partition_id, ctx)?;
                Ok(vacant_entry.insert(indexed_split))
            }
        }
    }

    fn create_workbench(&self) -> anyhow::Result<IndexingWorkbench> {
        let workbench = IndexingWorkbench {
            workbench_id: Ulid::new(),
            indexed_splits: FnvHashMap::with_capacity_and_hasher(250, Default::default()),
            checkpoint_delta: IndexCheckpointDelta {
                source_id: self.pipeline_id.source_id.clone(),
                source_delta: SourceCheckpointDelta::default(),
            },
            publish_lock: self.publish_lock.clone(),
            date_of_birth: Instant::now(),
        };
        Ok(workbench)
    }

    /// Returns the current_indexed_split. If this is the first message, then
    /// the indexed_split does not exist yet.
    ///
    /// This function will then create it, and can hence return an Error.
    async fn get_or_create_workbench<'a>(
        &'a self,
        indexing_workbench_opt: &'a mut Option<IndexingWorkbench>,
        ctx: &'a ActorContext<Indexer>,
    ) -> anyhow::Result<&'a mut IndexingWorkbench> {
        if indexing_workbench_opt.is_none() {
            let indexing_workbench = self.create_workbench()?;
            let commit_timeout_message = CommitTimeout {
                workbench_id: indexing_workbench.workbench_id,
            };
            ctx.schedule_self_msg(
                self.indexing_settings.commit_timeout(),
                commit_timeout_message,
            )
            .await;
            *indexing_workbench_opt = Some(indexing_workbench);
        }
        let current_indexing_workbench = indexing_workbench_opt.as_mut().context(
            "No index writer available. This should never happen! Please, report on https://github.com/quickwit-oss/quickwit/issues."
        )?;
        Ok(current_indexing_workbench)
    }

    fn prepare_document(&self, doc_json: String) -> PrepareDocumentOutcome {
        // Parse the document
        let doc_parsing_result = self.doc_mapper.doc_from_json(doc_json);
        let (partition, document) = match doc_parsing_result {
            Ok(doc) => doc,
            Err(doc_parsing_error) => {
                warn!(err=?doc_parsing_error);
                return match doc_parsing_error {
                    DocParsingError::RequiredFastField(_) => PrepareDocumentOutcome::MissingField,
                    _ => PrepareDocumentOutcome::ParsingError,
                };
            }
        };
        // Extract timestamp if necessary
        let timestamp_field = if let Some(timestamp_field) = self.timestamp_field_opt {
            timestamp_field
        } else {
            // No need to check the timestamp, there are no timestamp.
            return PrepareDocumentOutcome::Document {
                document,
                timestamp_opt: None,
                partition,
            };
        };
        let timestamp_opt = document
            .get_first(timestamp_field)
            .and_then(|value| match value {
                Value::Date(date_time) => Some(date_time.into_timestamp_secs()),
                value => value.as_i64(),
            });
        assert!(
            timestamp_opt.is_some(),
            "We should always have a timestamp here as doc parsing returns a `RequiredFastField` \
             error on a missing timestamp."
        );
        PrepareDocumentOutcome::Document {
            document,
            timestamp_opt,
            partition,
        }
    }

    async fn process_batch(
        &self,
        batch: RawDocBatch,
        indexing_workbench_opt: &mut Option<IndexingWorkbench>,
        counters: &mut IndexerCounters,
        ctx: &ActorContext<Indexer>,
    ) -> Result<(), ActorExitStatus> {
        let IndexingWorkbench {
            checkpoint_delta,
            indexed_splits,
            publish_lock,
            ..
        } = self
            .get_or_create_workbench(indexing_workbench_opt, ctx)
            .await?;
        if publish_lock.is_dead() {
            return Ok(());
        }
        checkpoint_delta
            .source_delta
            .extend(batch.checkpoint_delta)
            .context("Batch delta does not follow indexer checkpoint")?;
        for doc_json in batch.docs {
            let doc_json_num_bytes = doc_json.len() as u64;
            counters.overall_num_bytes += doc_json_num_bytes;
            let prepared_doc = {
                let _protect_zone = ctx.protect_zone();
                self.prepare_document(doc_json)
            };
            match prepared_doc {
                PrepareDocumentOutcome::ParsingError => {
                    counters.num_parse_errors += 1;
                }
                PrepareDocumentOutcome::MissingField => {
                    counters.num_missing_fields += 1;
                }
                PrepareDocumentOutcome::Document {
                    document,
                    timestamp_opt,
                    partition,
                } => {
                    let indexed_split =
                        self.get_or_create_indexed_split(partition, indexed_splits, ctx)?;
                    indexed_split.split_attrs.uncompressed_docs_size_in_bytes += doc_json_num_bytes;
                    counters.num_docs_in_workbench += 1;
                    counters.num_valid_docs += 1;
                    indexed_split.split_attrs.num_docs += 1;
                    if let Some(timestamp) = timestamp_opt {
                        record_timestamp(timestamp, &mut indexed_split.split_attrs.time_range);
                    }
                    let _protect_guard = ctx.protect_zone();
                    indexed_split
                        .index_writer
                        .add_document(document)
                        .context("Failed to add document.")?;
                }
            }
            ctx.record_progress();
        }
        Ok(())
    }
}

/// A workbench hosts the set of `IndexedSplit` that will are being built.
struct IndexingWorkbench {
    workbench_id: Ulid,
    indexed_splits: FnvHashMap<u64, IndexedSplit>,
    checkpoint_delta: IndexCheckpointDelta,
    publish_lock: PublishLock,
    // TODO create this Instant on the source side to be more accurate.
    // Right now this instant is used to compute time-to-search, but this
    // does not include the amount of time a document could have been
    // staying in the indexer queue or in the push api queue.
    date_of_birth: Instant,
}

pub struct Indexer {
    indexer_state: IndexerState,
    packager_mailbox: Mailbox<Packager>,
    indexing_workbench_opt: Option<IndexingWorkbench>,
    metastore: Arc<dyn Metastore>,
    counters: IndexerCounters,
}

#[async_trait]
impl Actor for Indexer {
    type ObservableState = IndexerCounters;

    fn observable_state(&self) -> Self::ObservableState {
        self.counters.clone()
    }

    fn queue_capacity(&self) -> QueueCapacity {
        QueueCapacity::Bounded(10)
    }

    fn name(&self) -> String {
        "Indexer".to_string()
    }

    fn runtime_handle(&self) -> Handle {
        RuntimeType::Blocking.get_runtime_handle()
    }

    async fn finalize(
        &mut self,
        exit_status: &ActorExitStatus,
        ctx: &ActorContext<Self>,
    ) -> anyhow::Result<()> {
        match exit_status {
            ActorExitStatus::DownstreamClosed
            | ActorExitStatus::Killed
            | ActorExitStatus::Failure(_)
            | ActorExitStatus::Panicked => return Ok(()),
            ActorExitStatus::Quit | ActorExitStatus::Success => {
                self.send_to_packager(CommitTrigger::NoMoreDocs, ctx)
                    .await?;
            }
        }
        Ok(())
    }
}

fn record_timestamp(timestamp: i64, time_range: &mut Option<RangeInclusive<i64>>) {
    let new_timestamp_range = match time_range.as_ref() {
        Some(range) => {
            RangeInclusive::new(timestamp.min(*range.start()), timestamp.max(*range.end()))
        }
        None => RangeInclusive::new(timestamp, timestamp),
    };
    *time_range = Some(new_timestamp_range);
}

#[async_trait]
impl Handler<CommitTimeout> for Indexer {
    type Reply = ();

    async fn handle(
        &mut self,
        commit_timeout: CommitTimeout,
        ctx: &ActorContext<Self>,
    ) -> Result<(), ActorExitStatus> {
        if let Some(indexing_workbench) = &self.indexing_workbench_opt {
            // If this is a timeout for a different workbench, we must ignore it.
            if indexing_workbench.workbench_id != commit_timeout.workbench_id {
                return Ok(());
            }
        }
        self.send_to_packager(CommitTrigger::Timeout, ctx).await?;
        Ok(())
    }
}

#[async_trait]
impl Handler<RawDocBatch> for Indexer {
    type Reply = ();

    async fn handle(
        &mut self,
        batch: RawDocBatch,
        ctx: &ActorContext<Self>,
    ) -> Result<(), ActorExitStatus> {
        self.process_batch(batch, ctx).await
    }
}

#[async_trait]
impl Handler<NewPublishLock> for Indexer {
    type Reply = ();

    async fn handle(
        &mut self,
        message: NewPublishLock,
        _ctx: &ActorContext<Self>,
    ) -> Result<(), ActorExitStatus> {
        let NewPublishLock(publish_lock) = message;
        self.indexing_workbench_opt = None;
        self.indexer_state.publish_lock = publish_lock;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
enum CommitTrigger {
    Timeout,
    NoMoreDocs,
    NumDocsLimit,
}

impl Indexer {
    pub fn new(
        pipeline_id: IndexingPipelineId,
        doc_mapper: Arc<dyn DocMapper>,
        metastore: Arc<dyn Metastore>,
        indexing_directory: IndexingDirectory,
        indexing_settings: IndexingSettings,
        packager_mailbox: Mailbox<Packager>,
    ) -> Self {
        let schema = doc_mapper.schema();
        let timestamp_field_opt = doc_mapper.timestamp_field(&schema);
        let sort_by_field_opt = match indexing_settings.sort_by() {
            SortBy::DocId | SortBy::Score { .. } => None,
            SortBy::FastField { field_name, order } => Some(IndexSortByField {
                field: field_name,
                order: order.into(),
            }),
        };
        let schema = doc_mapper.schema();
        let index_settings = IndexSettings {
            sort_by_field: sort_by_field_opt,
            docstore_blocksize: indexing_settings.docstore_blocksize,
            docstore_compression: Compressor::Zstd(ZstdCompressor {
                compression_level: Some(indexing_settings.docstore_compression_level),
            }),
        };
        let publish_lock = PublishLock::default();
        Self {
            indexer_state: IndexerState {
                pipeline_id,
                doc_mapper,
                indexing_directory,
                indexing_settings,
                publish_lock,
                timestamp_field_opt,
                schema,
                index_settings,
            },
            packager_mailbox,
            indexing_workbench_opt: None,
            metastore,
            counters: IndexerCounters::default(),
        }
    }

    async fn process_batch(
        &mut self,
        batch: RawDocBatch,
        ctx: &ActorContext<Self>,
    ) -> Result<(), ActorExitStatus> {
        fail_point!("indexer:batch:before");
        self.indexer_state
            .process_batch(
                batch,
                &mut self.indexing_workbench_opt,
                &mut self.counters,
                ctx,
            )
            .await?;
        if self.counters.num_docs_in_workbench
            >= self.indexer_state.indexing_settings.split_num_docs_target as u64
        {
            self.send_to_packager(CommitTrigger::NumDocsLimit, ctx)
                .await?;
        }
        fail_point!("indexer:batch:after");
        Ok(())
    }

    /// Extract the indexed split and send it to the Packager.
    async fn send_to_packager(
        &mut self,
        commit_trigger: CommitTrigger,
        ctx: &ActorContext<Self>,
    ) -> anyhow::Result<()> {
        let IndexingWorkbench {
            indexed_splits,
            checkpoint_delta,
            publish_lock,
            date_of_birth,
            ..
        } = if let Some(indexing_workbench) = self.indexing_workbench_opt.take() {
            indexing_workbench
        } else {
            return Ok(());
        };

        let splits: Vec<IndexedSplit> = indexed_splits.into_values().collect();

        // Avoid producing empty split, but still update the checkpoint to avoid
        // reprocessing the same faulty documents.
        if splits.is_empty() {
            if let Some(_guard) = publish_lock.acquire().await {
                ctx.protect_future(self.metastore.publish_splits(
                    &self.indexer_state.pipeline_id.index_id,
                    &[],
                    &[],
                    Some(checkpoint_delta),
                ))
                .await
                .with_context(|| {
                    format!(
                        "Failed to update the checkpoint for {}, {} after a split containing only \
                         errors.",
                        &self.indexer_state.pipeline_id.index_id,
                        &self.indexer_state.pipeline_id.source_id
                    )
                })?;
            } else {
                info!(
                    split_ids=?splits.iter().map(|split| split.split_id()).join(", "),
                    "Splits' publish lock is dead."
                );
                // TODO: Remove the junk right away?
            }
            return Ok(());
        }
        let num_splits = splits.len() as u64;
        let split_ids = splits.iter().map(|split| split.split_id()).join(",");
        info!(commit_trigger=?commit_trigger, split_ids=%split_ids, num_docs=self.counters.num_docs_in_workbench, "send-to-packager");
        ctx.send_message(
            &self.packager_mailbox,
            IndexedSplitBatch {
                splits,
                checkpoint_delta: Some(checkpoint_delta),
                publish_lock,
                date_of_birth,
            },
        )
        .await?;
        self.counters.num_docs_in_workbench = 0;
        self.counters.num_splits_emitted += num_splits;
        self.counters.num_split_batches_emitted += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use quickwit_actors::{create_test_mailbox, Universe};
    use quickwit_doc_mapper::{default_doc_mapper_for_test, DefaultDocMapper, SortOrder};
    use quickwit_metastore::checkpoint::SourceCheckpointDelta;
    use quickwit_metastore::MockMetastore;

    use super::*;
    use crate::actors::indexer::{record_timestamp, IndexerCounters};
    use crate::models::{IndexingDirectory, RawDocBatch};

    #[test]
    fn test_record_timestamp() {
        let mut time_range = None;
        record_timestamp(1628664679, &mut time_range);
        assert_eq!(time_range, Some(1628664679..=1628664679));
        record_timestamp(1628664112, &mut time_range);
        assert_eq!(time_range, Some(1628664112..=1628664679));
        record_timestamp(1628665112, &mut time_range);
        assert_eq!(time_range, Some(1628664112..=1628665112))
    }

    #[tokio::test]
    async fn test_indexer_simple() -> anyhow::Result<()> {
        let pipeline_id = IndexingPipelineId {
            index_id: "test-index".to_string(),
            source_id: "test-source".to_string(),
            node_id: "test-node".to_string(),
            pipeline_ord: 0,
        };
        let doc_mapper = Arc::new(default_doc_mapper_for_test());
        let indexing_directory = IndexingDirectory::for_test().await?;
        let mut indexing_settings = IndexingSettings::for_test();
        indexing_settings.split_num_docs_target = 3;
        indexing_settings.sort_field = Some("timestamp".to_string());
        indexing_settings.sort_order = Some(SortOrder::Desc);
        indexing_settings.timestamp_field = Some("timestamp".to_string());
        let (packager_mailbox, packager_inbox) = create_test_mailbox();
        let mut metastore = MockMetastore::default();
        metastore
            .expect_publish_splits()
            .returning(move |_, splits, _, _| {
                assert!(splits.is_empty());
                Ok(())
            });
        let indexer = Indexer::new(
            pipeline_id,
            doc_mapper,
            Arc::new(metastore),
            indexing_directory,
            indexing_settings,
            packager_mailbox,
        );
        let universe = Universe::new();
        let (indexer_mailbox, indexer_handle) = universe.spawn_actor(indexer).spawn();
        indexer_mailbox
            .send_message(RawDocBatch {
                docs: vec![
                        r#"{"body": "happy", "response_date": "2021-12-19T16:39:57+00:00", "response_time": 12, "response_payload": "YWJj"}"#.to_string(), // missing timestamp
                        r#"{"body": "happy", "timestamp": 1628837062, "response_date": "2021-12-19T16:39:59+00:00", "response_time": 2, "response_payload": "YWJj"}"#.to_string(), // ok
                        r#"{"body": "happy2", "timestamp": 1628837062, "response_date": "2021-12-19T16:40:57+00:00", "response_time": 13, "response_payload": "YWJj"}"#.to_string(), // ok
                        "{".to_string(),                    // invalid json
                    ],
                checkpoint_delta: SourceCheckpointDelta::from(0..4),
            })
            .await?;
        let indexer_counters = indexer_handle.process_pending_and_observe().await.state;
        assert_eq!(
            indexer_counters,
            IndexerCounters {
                num_parse_errors: 1,
                num_missing_fields: 1,
                num_valid_docs: 2,
                num_splits_emitted: 0,
                num_split_batches_emitted: 0,
                num_docs_in_workbench: 2, //< we have not reached the commit limit yet.
                overall_num_bytes: 387
            }
        );
        indexer_mailbox
            .send_message(
                RawDocBatch {
                    docs: vec![r#"{"body": "happy3", "timestamp": 1628837062, "response_date": "2021-12-19T16:39:57+00:00", "response_time": 12, "response_payload": "YWJj"}"#.to_string()],
                    checkpoint_delta: SourceCheckpointDelta::from(4..5),
                }
            )
            .await?;
        let indexer_counters = indexer_handle.process_pending_and_observe().await.state;
        assert_eq!(
            indexer_counters,
            IndexerCounters {
                num_parse_errors: 1,
                num_missing_fields: 1,
                num_valid_docs: 3,
                num_splits_emitted: 1,
                num_split_batches_emitted: 1,
                num_docs_in_workbench: 0, //< the num docs in split counter has been reset.
                overall_num_bytes: 525
            }
        );
        let output_messages = packager_inbox.drain_for_test();
        assert_eq!(output_messages.len(), 1);
        let batch = output_messages[0]
            .downcast_ref::<IndexedSplitBatch>()
            .unwrap();
        assert_eq!(batch.splits[0].split_attrs.num_docs, 3);
        let sort_by_field = batch.splits[0].index.settings().sort_by_field.as_ref();
        assert!(sort_by_field.is_some());
        assert_eq!(sort_by_field.unwrap().field, "timestamp");
        assert!(sort_by_field.unwrap().order.is_desc());
        Ok(())
    }

    #[tokio::test]
    async fn test_indexer_timeout() -> anyhow::Result<()> {
        let pipeline_id = IndexingPipelineId {
            index_id: "test-index".to_string(),
            source_id: "test-source".to_string(),
            node_id: "test-node".to_string(),
            pipeline_ord: 0,
        };
        let doc_mapper = Arc::new(default_doc_mapper_for_test());
        let indexing_directory = IndexingDirectory::for_test().await?;
        let indexing_settings = IndexingSettings::for_test();
        let (packager_mailbox, packager_inbox) = create_test_mailbox();
        let mut metastore = MockMetastore::default();
        metastore
            .expect_publish_splits()
            .returning(move |_, splits, _, _| {
                assert!(splits.is_empty());
                Ok(())
            });
        let indexer = Indexer::new(
            pipeline_id,
            doc_mapper,
            Arc::new(metastore),
            indexing_directory,
            indexing_settings,
            packager_mailbox,
        );
        let universe = Universe::new();
        let (indexer_mailbox, indexer_handle) = universe.spawn_actor(indexer).spawn();
        indexer_mailbox
            .send_message(
                RawDocBatch {
                    docs: vec![r#"{"body": "happy", "timestamp": 1628837062, "response_date": "2021-12-19T16:39:57+00:00", "response_time": 12, "response_payload": "YWJj"}"#.to_string()],
                    checkpoint_delta: SourceCheckpointDelta::from(0..1),
                }
            )
            .await?;
        let indexer_counters = indexer_handle.process_pending_and_observe().await.state;
        assert_eq!(
            indexer_counters,
            IndexerCounters {
                num_parse_errors: 0,
                num_missing_fields: 0,
                num_valid_docs: 1,
                num_splits_emitted: 0,
                num_split_batches_emitted: 0,
                num_docs_in_workbench: 1,
                overall_num_bytes: 137
            }
        );
        universe.simulate_time_shift(Duration::from_secs(61)).await;
        let indexer_counters = indexer_handle.process_pending_and_observe().await.state;
        assert_eq!(
            indexer_counters,
            IndexerCounters {
                num_parse_errors: 0,
                num_missing_fields: 0,
                num_valid_docs: 1,
                num_splits_emitted: 1,
                num_split_batches_emitted: 1,
                num_docs_in_workbench: 0,
                overall_num_bytes: 137
            }
        );
        let output_messages = packager_inbox.drain_for_test();
        assert_eq!(output_messages.len(), 1);
        let indexed_split_batch = output_messages[0]
            .downcast_ref::<IndexedSplitBatch>()
            .unwrap();
        assert_eq!(indexed_split_batch.splits[0].split_attrs.num_docs, 1);
        Ok(())
    }

    #[tokio::test]
    async fn test_indexer_eof() -> anyhow::Result<()> {
        let pipeline_id = IndexingPipelineId {
            index_id: "test-index".to_string(),
            source_id: "test-source".to_string(),
            node_id: "test-node".to_string(),
            pipeline_ord: 0,
        };
        let doc_mapper = Arc::new(default_doc_mapper_for_test());
        let indexing_directory = IndexingDirectory::for_test().await?;
        let indexing_settings = IndexingSettings::for_test();
        let (packager_mailbox, packager_inbox) = create_test_mailbox();
        let mut metastore = MockMetastore::default();
        metastore
            .expect_publish_splits()
            .returning(move |_, splits, _, _| {
                assert!(splits.is_empty());
                Ok(())
            });
        let indexer = Indexer::new(
            pipeline_id,
            doc_mapper,
            Arc::new(metastore),
            indexing_directory,
            indexing_settings,
            packager_mailbox,
        );
        let universe = Universe::new();
        let (indexer_mailbox, indexer_handle) = universe.spawn_actor(indexer).spawn();
        indexer_mailbox
            .send_message(
                RawDocBatch {
                    docs: vec![r#"{"body": "happy", "timestamp": 1628837062, "response_date": "2021-12-19T16:39:57+00:00", "response_time": 12, "response_payload": "YWJj"}"#.to_string()],
                    checkpoint_delta: SourceCheckpointDelta::from(0..1),
                }
            )
            .await?;
        universe.send_exit_with_success(&indexer_mailbox).await?;
        let (exit_status, indexer_counters) = indexer_handle.join().await;
        assert!(exit_status.is_success());
        assert_eq!(
            indexer_counters,
            IndexerCounters {
                num_parse_errors: 0,
                num_missing_fields: 0,
                num_valid_docs: 1,
                num_splits_emitted: 1,
                num_split_batches_emitted: 1,
                num_docs_in_workbench: 0,
                overall_num_bytes: 137
            }
        );
        let output_messages = packager_inbox.drain_for_test();
        assert_eq!(output_messages.len(), 1);
        assert_eq!(
            output_messages[0]
                .downcast_ref::<IndexedSplitBatch>()
                .unwrap()
                .splits[0]
                .split_attrs
                .num_docs,
            1
        );
        Ok(())
    }

    const DOCMAPPER_WITH_PARTITION_JSON: &str = r#"
        {
            "tag_fields": ["tenant"],
            "partition_key": "tenant",
            "field_mappings": [
                { "name": "tenant", "type": "text", "tokenizer": "raw", "indexed": true },
                { "name": "body", "type": "text" }
            ]
        }"#;

    #[tokio::test]
    async fn test_indexer_partitioning() -> anyhow::Result<()> {
        let pipeline_id = IndexingPipelineId {
            index_id: "test-index".to_string(),
            source_id: "test-source".to_string(),
            node_id: "test-node".to_string(),
            pipeline_ord: 0,
        };
        let doc_mapper: Arc<dyn DocMapper> = Arc::new(
            serde_json::from_str::<DefaultDocMapper>(DOCMAPPER_WITH_PARTITION_JSON).unwrap(),
        );
        let indexing_directory = IndexingDirectory::for_test().await?;
        let indexing_settings = IndexingSettings::for_test();
        let (packager_mailbox, packager_inbox) = create_test_mailbox();
        let mut metastore = MockMetastore::default();
        metastore
            .expect_publish_splits()
            .returning(move |_, splits, _, _| {
                assert!(splits.is_empty());
                Ok(())
            });

        let indexer = Indexer::new(
            pipeline_id,
            doc_mapper,
            Arc::new(metastore),
            indexing_directory,
            indexing_settings,
            packager_mailbox,
        );
        let universe = Universe::new();
        let (indexer_mailbox, indexer_handle) = universe.spawn_actor(indexer).spawn();
        indexer_mailbox
            .send_message(RawDocBatch {
                docs: vec![
                    r#"{"tenant": "tenant_1", "body": "first doc for tenant 1"}"#.to_string(),
                    r#"{"tenant": "tenant_2", "body": "first doc for tenant 2"}"#.to_string(),
                    r#"{"tenant": "tenant_1", "body": "second doc for tenant 1"}"#.to_string(),
                ],
                checkpoint_delta: SourceCheckpointDelta::from(0..2),
            })
            .await?;

        let indexer_counters = indexer_handle.process_pending_and_observe().await.state;
        assert_eq!(
            indexer_counters,
            IndexerCounters {
                num_parse_errors: 0,
                num_missing_fields: 0,
                num_valid_docs: 3,
                num_docs_in_workbench: 3,
                num_splits_emitted: 0,
                num_split_batches_emitted: 0,
                overall_num_bytes: 169
            }
        );
        universe.send_exit_with_success(&indexer_mailbox).await?;
        let (exit_status, indexer_counters) = indexer_handle.join().await;
        assert!(matches!(exit_status, ActorExitStatus::Success));
        assert_eq!(
            indexer_counters,
            IndexerCounters {
                num_parse_errors: 0,
                num_missing_fields: 0,
                num_valid_docs: 3,
                num_docs_in_workbench: 0,
                num_splits_emitted: 2,
                num_split_batches_emitted: 1,
                overall_num_bytes: 169
            }
        );

        let output_messages = packager_inbox.drain_for_test();
        assert_eq!(output_messages.len(), 1);

        let indexed_split_batch = output_messages[0]
            .downcast_ref::<IndexedSplitBatch>()
            .unwrap();
        assert_eq!(indexed_split_batch.splits.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn test_indexer_propagates_publish_lock() {
        let pipeline_id = IndexingPipelineId {
            index_id: "test-index".to_string(),
            source_id: "test-source".to_string(),
            node_id: "test-node".to_string(),
            pipeline_ord: 0,
        };
        let doc_mapper = Arc::new(default_doc_mapper_for_test());
        let indexing_directory = IndexingDirectory::for_test().await.unwrap();
        let mut indexing_settings = IndexingSettings::for_test();
        indexing_settings.split_num_docs_target = 1;
        let mut metastore = MockMetastore::default();
        metastore.expect_publish_splits().never();
        let (packager_mailbox, packager_inbox) = create_test_mailbox();
        let indexer = Indexer::new(
            pipeline_id,
            doc_mapper,
            Arc::new(metastore),
            indexing_directory,
            indexing_settings,
            packager_mailbox,
        );
        let universe = Universe::new();
        let (indexer_mailbox, indexer_handle) = universe.spawn_actor(indexer).spawn();

        for id in ["foo-publish-lock", "bar-publish-lock"] {
            let publish_lock = PublishLock::for_test(true, id);

            indexer_mailbox
                .send_message(NewPublishLock(publish_lock))
                .await
                .unwrap();

            indexer_mailbox
            .send_message(RawDocBatch {
                docs: vec![
                        r#"{"body": "happy", "timestamp": 1628837062, "response_date": "2021-12-19T16:39:59+00:00", "response_time": 2, "response_payload": "YWJj"}"#.to_string(),
                    ],
                checkpoint_delta: SourceCheckpointDelta::from(0..1),
            })
            .await.unwrap();
        }
        universe
            .send_exit_with_success(&indexer_mailbox)
            .await
            .unwrap();
        let (exit_status, _indexer_counters) = indexer_handle.join().await;
        assert!(matches!(exit_status, ActorExitStatus::Success));

        let packager_messages: Vec<IndexedSplitBatch> = packager_inbox.drain_for_test_typed();
        assert_eq!(packager_messages.len(), 2);
        assert_eq!(packager_messages[0].splits.len(), 1);
        assert_eq!(packager_messages[0].publish_lock.id, "foo-publish-lock");
        assert_eq!(packager_messages[1].splits.len(), 1);
        assert_eq!(packager_messages[1].publish_lock.id, "bar-publish-lock");
    }

    #[tokio::test]
    async fn test_indexer_ignores_messages_when_publish_lock_is_dead() {
        let pipeline_id = IndexingPipelineId {
            index_id: "test-index".to_string(),
            source_id: "test-source".to_string(),
            node_id: "test-node".to_string(),
            pipeline_ord: 0,
        };
        let doc_mapper = Arc::new(default_doc_mapper_for_test());
        let indexing_directory = IndexingDirectory::for_test().await.unwrap();
        let mut indexing_settings = IndexingSettings::for_test();
        indexing_settings.split_num_docs_target = 1;
        let mut metastore = MockMetastore::default();
        metastore.expect_publish_splits().never();
        let (packager_mailbox, packager_inbox) = create_test_mailbox();
        let indexer = Indexer::new(
            pipeline_id,
            doc_mapper,
            Arc::new(metastore),
            indexing_directory,
            indexing_settings,
            packager_mailbox,
        );
        let universe = Universe::new();
        let (indexer_mailbox, indexer_handle) = universe.spawn_actor(indexer).spawn();

        let publish_lock = PublishLock::for_test(false, "foo-publish-lock");

        indexer_mailbox
            .send_message(NewPublishLock(publish_lock))
            .await
            .unwrap();

        indexer_mailbox
            .send_message(RawDocBatch {
                docs: vec![
                        r#"{"body": "happy", "timestamp": 1628837062, "response_date": "2021-12-19T16:39:59+00:00", "response_time": 2, "response_payload": "YWJj"}"#.to_string(),
                    ],
                checkpoint_delta: SourceCheckpointDelta::from(0..1),
            })
            .await.unwrap();

        universe
            .send_exit_with_success(&indexer_mailbox)
            .await
            .unwrap();
        let (exit_status, _indexer_counters) = indexer_handle.join().await;
        assert!(matches!(exit_status, ActorExitStatus::Success));

        let packager_messages = packager_inbox.drain_for_test();
        assert!(packager_messages.is_empty());
    }

    #[tokio::test]
    async fn test_indexer_acquires_publish_lock() {
        let pipeline_id = IndexingPipelineId {
            index_id: "test-index".to_string(),
            source_id: "test-source".to_string(),
            node_id: "test-node".to_string(),
            pipeline_ord: 0,
        };
        let doc_mapper = Arc::new(default_doc_mapper_for_test());
        let indexing_directory = IndexingDirectory::for_test().await.unwrap();
        let indexing_settings = IndexingSettings::for_test();
        let mut metastore = MockMetastore::default();
        metastore.expect_publish_splits().never();
        let (packager_mailbox, packager_inbox) = create_test_mailbox();
        let indexer = Indexer::new(
            pipeline_id,
            doc_mapper,
            Arc::new(metastore),
            indexing_directory,
            indexing_settings,
            packager_mailbox,
        );
        let universe = Universe::new();
        let (indexer_mailbox, indexer_handle) = universe.spawn_actor(indexer).spawn();

        let publish_lock = PublishLock::for_test(true, "foo-publish-lock");

        indexer_mailbox
            .send_message(NewPublishLock(publish_lock.clone()))
            .await
            .unwrap();

        indexer_mailbox
            .send_message(RawDocBatch {
                docs: vec![
                    "{".to_string(), // Bad JSON
                ],
                checkpoint_delta: SourceCheckpointDelta::from(0..1),
            })
            .await
            .unwrap();

        publish_lock.kill().await;

        universe
            .send_exit_with_success(&indexer_mailbox)
            .await
            .unwrap();
        let (exit_status, _indexer_counters) = indexer_handle.join().await;
        assert!(matches!(exit_status, ActorExitStatus::Success));

        let packager_messages = packager_inbox.drain_for_test();
        assert!(packager_messages.is_empty());
    }
}
