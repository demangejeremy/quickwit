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

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use quickwit_actors::{
    create_mailbox, Actor, ActorContext, ActorExitStatus, ActorHandle, Handler, Health, KillSwitch,
    QueueCapacity, Supervisable,
};
use quickwit_config::{build_doc_mapper, IndexingSettings, SourceConfig};
use quickwit_doc_mapper::DocMapper;
use quickwit_metastore::{IndexMetadata, Metastore, MetastoreError, SplitState};
use quickwit_storage::Storage;
use tokio::join;
use tracing::{debug, error, info, info_span, instrument, Span};

use crate::actors::merge_split_downloader::MergeSplitDownloader;
use crate::actors::publisher::PublisherType;
use crate::actors::sequencer::Sequencer;
use crate::actors::{
    GarbageCollector, Indexer, MergeExecutor, MergePlanner, NamedField, Packager, Publisher,
    Uploader,
};
use crate::models::{IndexingDirectory, IndexingPipelineId, IndexingStatistics, Observe};
use crate::source::{quickwit_supported_sources, SourceActor, SourceExecutionContext};
use crate::split_store::{IndexingSplitStore, IndexingSplitStoreParams};
use crate::{MergePolicy, StableMultitenantWithTimestampMergePolicy};

const MAX_RETRY_DELAY: Duration = Duration::from_secs(600); // 10 min.

pub struct IndexingPipelineHandle {
    /// Indexing pipeline
    pub source: ActorHandle<SourceActor>,
    pub indexer: ActorHandle<Indexer>,
    pub packager: ActorHandle<Packager>,
    pub uploader: ActorHandle<Uploader>,
    pub sequencer: ActorHandle<Sequencer<Publisher>>,
    pub publisher: ActorHandle<Publisher>,
    pub garbage_collector: ActorHandle<GarbageCollector>,

    /// Merging pipeline subpipeline
    pub merge_planner: ActorHandle<MergePlanner>,
    pub merge_split_downloader: ActorHandle<MergeSplitDownloader>,
    pub merge_executor: ActorHandle<MergeExecutor>,
    pub merge_packager: ActorHandle<Packager>,
    pub merge_uploader: ActorHandle<Uploader>,
    pub merge_sequencer: ActorHandle<Sequencer<Publisher>>,
    pub merge_publisher: ActorHandle<Publisher>,
}

// Messages

#[derive(Clone, Copy, Debug)]
pub struct Supervise;

#[derive(Clone, Copy, Debug, Default)]
pub struct Spawn {
    retry_count: usize,
}

pub struct IndexingPipeline {
    params: IndexingPipelineParams,
    previous_generations_statistics: IndexingStatistics,
    statistics: IndexingStatistics,
    handles: Option<IndexingPipelineHandle>,
    // Killswitch used for the actors in the pipeline. This is not the supervisor killswitch.
    kill_switch: KillSwitch,
}

#[async_trait]
impl Actor for IndexingPipeline {
    type ObservableState = IndexingStatistics;

    fn observable_state(&self) -> Self::ObservableState {
        self.statistics.clone()
    }

    fn name(&self) -> String {
        "IndexingPipeline".to_string()
    }

    fn span(&self, _ctx: &ActorContext<Self>) -> Span {
        info_span!("")
    }

    async fn initialize(&mut self, ctx: &ActorContext<Self>) -> Result<(), ActorExitStatus> {
        self.handle(Spawn::default(), ctx).await?;
        self.handle(Observe, ctx).await?;
        self.handle(Supervise, ctx).await?;
        Ok(())
    }
}

impl IndexingPipeline {
    pub fn new(params: IndexingPipelineParams) -> Self {
        Self {
            params,
            previous_generations_statistics: Default::default(),
            handles: None,
            kill_switch: KillSwitch::default(),
            statistics: IndexingStatistics::default(),
        }
    }

    fn supervisables(&self) -> Vec<&dyn Supervisable> {
        if let Some(handles) = &self.handles {
            let supervisables: Vec<&dyn Supervisable> = vec![
                &handles.source,
                &handles.indexer,
                &handles.packager,
                &handles.uploader,
                &handles.sequencer,
                &handles.publisher,
                &handles.garbage_collector,
                &handles.merge_planner,
                &handles.merge_split_downloader,
                &handles.merge_executor,
                &handles.merge_packager,
                &handles.merge_uploader,
                &handles.merge_sequencer,
                &handles.merge_publisher,
            ];
            supervisables
        } else {
            Vec::new()
        }
    }

    /// Performs healthcheck on all of the actors in the pipeline,
    /// and consolidates the result.
    fn healthcheck(&self) -> Health {
        let mut healthy_actors: Vec<&str> = Default::default();
        let mut failure_or_unhealthy_actors: Vec<&str> = Default::default();
        let mut success_actors: Vec<&str> = Default::default();
        for supervisable in self.supervisables() {
            match supervisable.health() {
                Health::Healthy => {
                    // At least one other actor is running.
                    healthy_actors.push(supervisable.name());
                }
                Health::FailureOrUnhealthy => {
                    failure_or_unhealthy_actors.push(supervisable.name());
                }
                Health::Success => {
                    success_actors.push(supervisable.name());
                }
            }
        }

        if !failure_or_unhealthy_actors.is_empty() {
            error!(
                pipeline_id=?self.params.pipeline_id,
                generation=self.generation(),
                healthy_actors=?healthy_actors,
                failed_or_unhealthy_actors=?failure_or_unhealthy_actors,
                success_actors=?success_actors,
                "Indexing pipeline failure."
            );
            return Health::FailureOrUnhealthy;
        }
        if healthy_actors.is_empty() {
            // All the actors finished successfully.
            info!(
                pipeline_id=?self.params.pipeline_id,
                generation=self.generation(),
                "Indexing pipeline success."
            );
            return Health::Success;
        }
        // No error at this point and there are still some actors running.
        debug!(
            pipeline_id=?self.params.pipeline_id,
            generation=self.generation(),
            healthy_actors=?healthy_actors,
            failed_or_unhealthy_actors=?failure_or_unhealthy_actors,
            success_actors=?success_actors,
            "Indexing pipeline running."
        );
        Health::Healthy
    }

    fn generation(&self) -> usize {
        self.statistics.generation
    }

    // TODO this should return an error saying whether we can retry or not.
    #[instrument(name="", level="info", skip_all, fields(index=%self.params.pipeline_id.index_id, gen=self.generation()))]
    async fn spawn_pipeline(&mut self, ctx: &ActorContext<Self>) -> anyhow::Result<()> {
        self.statistics.num_spawn_attempts += 1;
        self.kill_switch = KillSwitch::default();
        let stable_multitenant_merge_policy = StableMultitenantWithTimestampMergePolicy {
            merge_enabled: self.params.indexing_settings.merge_enabled,
            merge_factor: self.params.indexing_settings.merge_policy.merge_factor,
            max_merge_factor: self.params.indexing_settings.merge_policy.max_merge_factor,
            split_num_docs_target: self.params.indexing_settings.split_num_docs_target,
            ..Default::default()
        };
        let merge_policy: Arc<dyn MergePolicy> = Arc::new(stable_multitenant_merge_policy);
        info!(
            index_id=%self.params.pipeline_id.index_id,
            source_id=%self.params.pipeline_id.source_id,
            pipeline_ord=%self.params.pipeline_id.pipeline_ord,
            root_dir=%self.params.indexing_directory.path().display(),
            merge_policy=?merge_policy,
            "Spawning indexing pipeline.",
        );
        let split_store = IndexingSplitStore::create_with_local_store(
            self.params.storage.clone(),
            self.params.indexing_directory.cache_directory.as_path(),
            IndexingSplitStoreParams {
                max_num_bytes: self.params.split_store_max_num_bytes,
                max_num_splits: self.params.split_store_max_num_splits,
            },
            merge_policy.clone(),
        )?;
        let published_splits = self
            .params
            .metastore
            .list_splits(
                &self.params.pipeline_id.index_id,
                SplitState::Published,
                None,
                None,
            )
            .await?
            .into_iter()
            .map(|split| split.split_metadata)
            .collect::<Vec<_>>();
        split_store
            .remove_dangling_splits(&published_splits)
            .await?;

        let (merge_planner_mailbox, merge_planner_inbox) =
            create_mailbox::<MergePlanner>("MergePlanner".to_string(), QueueCapacity::Unbounded);

        // Garbage colletor
        let garbage_collector = GarbageCollector::new(
            self.params.pipeline_id.clone(),
            split_store.clone(),
            self.params.metastore.clone(),
        );
        let (garbage_collector_mailbox, garbage_collector_handler) = ctx
            .spawn_actor(garbage_collector)
            .set_kill_switch(self.kill_switch.clone())
            .spawn();

        // Merge publisher
        let merge_publisher = Publisher::new(
            PublisherType::MergePublisher,
            self.params.metastore.clone(),
            merge_planner_mailbox.clone(),
            garbage_collector_mailbox.clone(),
            None,
        );
        let (merge_publisher_mailbox, merge_publisher_handler) = ctx
            .spawn_actor(merge_publisher)
            .set_kill_switch(self.kill_switch.clone())
            .spawn();

        let merge_sequencer = Sequencer::new(merge_publisher_mailbox);
        let (merge_sequencer_mailbox, merge_sequencer_handler) = ctx
            .spawn_actor(merge_sequencer)
            .set_kill_switch(self.kill_switch.clone())
            .spawn();

        // Merge uploader
        let merge_uploader = Uploader::new(
            "MergeUploader",
            self.params.metastore.clone(),
            split_store.clone(),
            merge_sequencer_mailbox,
        );
        let (merge_uploader_mailbox, merge_uploader_handler) = ctx
            .spawn_actor(merge_uploader)
            .set_kill_switch(self.kill_switch.clone())
            .spawn();

        // Merge Packager
        let index_schema = self.params.doc_mapper.schema();
        let tag_fields = self
            .params
            .doc_mapper
            .tag_field_names()
            .iter()
            .map(|field_name| {
                index_schema
                    .get_field(field_name)
                    .context(format!("Field `{}` must exist in the schema.", field_name))
                    .map(|field| NamedField {
                        name: field_name.clone(),
                        field,
                        field_type: index_schema.get_field_entry(field).field_type().clone(),
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let merge_packager =
            Packager::new("MergePackager", tag_fields.clone(), merge_uploader_mailbox);
        let (merge_packager_mailbox, merge_packager_handler) = ctx
            .spawn_actor(merge_packager)
            .set_kill_switch(self.kill_switch.clone())
            .spawn();

        let merge_executor =
            MergeExecutor::new(self.params.pipeline_id.clone(), merge_packager_mailbox);
        let (merge_executor_mailbox, merge_executor_handler) = ctx
            .spawn_actor(merge_executor)
            .set_kill_switch(self.kill_switch.clone())
            .spawn();

        let merge_split_downloader = MergeSplitDownloader {
            scratch_directory: self.params.indexing_directory.scratch_directory.clone(),
            storage: split_store.clone(),
            merge_executor_mailbox,
        };
        let (merge_split_downloader_mailbox, merge_split_downloader_handler) = ctx
            .spawn_actor(merge_split_downloader)
            .set_kill_switch(self.kill_switch.clone())
            .spawn();

        // Merge planner
        let merge_planner = MergePlanner::new(
            self.params.pipeline_id.clone(),
            published_splits,
            merge_policy.clone(),
            merge_split_downloader_mailbox,
        );
        let (merge_planner_mailbox, merge_planner_handler) = ctx
            .spawn_actor(merge_planner)
            .set_kill_switch(self.kill_switch.clone())
            .set_mailboxes(merge_planner_mailbox, merge_planner_inbox)
            .spawn();

        let (source_mailbox, source_inbox) =
            create_mailbox::<SourceActor>("SourceActor".to_string(), QueueCapacity::Unbounded);

        // Publisher
        let publisher = Publisher::new(
            PublisherType::MainPublisher,
            self.params.metastore.clone(),
            merge_planner_mailbox,
            garbage_collector_mailbox,
            Some(source_mailbox.clone()),
        );
        let (publisher_mailbox, publisher_handler) = ctx
            .spawn_actor(publisher)
            .set_kill_switch(self.kill_switch.clone())
            .spawn();

        let sequencer = Sequencer::new(publisher_mailbox);
        let (sequencer_mailbox, sequencer_handler) = ctx
            .spawn_actor(sequencer)
            .set_kill_switch(self.kill_switch.clone())
            .spawn();

        // Uploader
        let uploader = Uploader::new(
            "Uploader",
            self.params.metastore.clone(),
            split_store.clone(),
            sequencer_mailbox,
        );
        let (uploader_mailbox, uploader_handler) = ctx
            .spawn_actor(uploader)
            .set_kill_switch(self.kill_switch.clone())
            .spawn();

        // Packager
        let packager = Packager::new("Packager", tag_fields, uploader_mailbox);
        let (packager_mailbox, packager_handler) = ctx
            .spawn_actor(packager)
            .set_kill_switch(self.kill_switch.clone())
            .spawn();
        // Indexer
        let indexer = Indexer::new(
            self.params.pipeline_id.clone(),
            self.params.doc_mapper.clone(),
            self.params.metastore.clone(),
            self.params.indexing_directory.clone(),
            self.params.indexing_settings.clone(),
            packager_mailbox,
        );
        let (indexer_mailbox, indexer_handler) = ctx
            .spawn_actor(indexer)
            .set_kill_switch(self.kill_switch.clone())
            .spawn();

        // Fetch index_metadata to be sure to have the last updated checkpoint.
        let index_metadata = self
            .params
            .metastore
            .index_metadata(&self.params.pipeline_id.index_id)
            .await?;
        let source_checkpoint = index_metadata
            .checkpoint
            .source_checkpoint(&self.params.pipeline_id.source_id)
            .cloned()
            .unwrap_or_default(); // TODO Have a stricter check.
        let source = quickwit_supported_sources()
            .load_source(
                Arc::new(SourceExecutionContext {
                    metastore: self.params.metastore.clone(),
                    index_id: self.params.pipeline_id.index_id.clone(),
                    source_config: self.params.source_config.clone(),
                }),
                source_checkpoint,
            )
            .await?;
        let actor_source = SourceActor {
            source,
            indexer_mailbox,
        };
        let (_source_mailbox, source_handler) = ctx
            .spawn_actor(actor_source)
            .set_mailboxes(source_mailbox, source_inbox)
            .set_kill_switch(self.kill_switch.clone())
            .spawn();

        // Increment generation once we are sure there will be no spawning error.
        self.previous_generations_statistics = self.statistics.clone();
        self.statistics.generation += 1;
        self.handles = Some(IndexingPipelineHandle {
            source: source_handler,
            indexer: indexer_handler,
            packager: packager_handler,
            uploader: uploader_handler,
            sequencer: sequencer_handler,
            publisher: publisher_handler,
            garbage_collector: garbage_collector_handler,

            merge_planner: merge_planner_handler,
            merge_split_downloader: merge_split_downloader_handler,
            merge_executor: merge_executor_handler,
            merge_packager: merge_packager_handler,
            merge_uploader: merge_uploader_handler,
            merge_sequencer: merge_sequencer_handler,
            merge_publisher: merge_publisher_handler,
        });
        Ok(())
    }

    // retry_count, wait_time
    // 0   2s
    // 1   4s
    // 2   8s
    // 3   16s
    // ...
    // >=8   5mn
    fn wait_duration_before_retry(retry_count: usize) -> Duration {
        // Protect against a `retry_count` that will lead to an overflow.
        let max_power = (retry_count as u32 + 1).min(31);
        Duration::from_secs(2u64.pow(max_power) as u64).min(MAX_RETRY_DELAY)
    }

    async fn terminate(&mut self) {
        self.kill_switch.kill();
        if let Some(handlers) = self.handles.take() {
            tokio::join!(
                handlers.source.kill(),
                handlers.indexer.kill(),
                handlers.packager.kill(),
                handlers.uploader.kill(),
                handlers.publisher.kill(),
                handlers.garbage_collector.kill(),
                handlers.merge_planner.kill(),
                handlers.merge_split_downloader.kill(),
                handlers.merge_executor.kill(),
                handlers.merge_packager.kill(),
                handlers.merge_uploader.kill(),
                handlers.merge_publisher.kill(),
            );
        }
    }
}

#[async_trait]
impl Handler<Observe> for IndexingPipeline {
    type Reply = ();
    async fn handle(
        &mut self,
        _: Observe,
        ctx: &ActorContext<Self>,
    ) -> Result<(), ActorExitStatus> {
        if let Some(handles) = &self.handles {
            let (indexer_counters, uploader_counters, publisher_counters) = join!(
                handles.indexer.observe(),
                handles.uploader.observe(),
                handles.publisher.observe(),
            );
            self.statistics = self
                .previous_generations_statistics
                .clone()
                .add_actor_counters(
                    &*indexer_counters,
                    &*uploader_counters,
                    &*publisher_counters,
                )
                .set_generation(self.statistics.generation)
                .set_num_spawn_attempts(self.statistics.num_spawn_attempts);
        }
        ctx.schedule_self_msg(Duration::from_secs(1), Observe).await;
        Ok(())
    }
}

#[async_trait]
impl Handler<Supervise> for IndexingPipeline {
    type Reply = ();

    async fn handle(
        &mut self,
        _: Supervise,
        ctx: &ActorContext<Self>,
    ) -> Result<(), ActorExitStatus> {
        if self.handles.is_some() {
            match self.healthcheck() {
                Health::Healthy => {}
                Health::FailureOrUnhealthy => {
                    self.terminate().await;
                    ctx.schedule_self_msg(quickwit_actors::HEARTBEAT, Spawn { retry_count: 0 })
                        .await;
                }
                Health::Success => {
                    return Err(ActorExitStatus::Success);
                }
            }
        }
        ctx.schedule_self_msg(quickwit_actors::HEARTBEAT, Supervise)
            .await;
        Ok(())
    }
}

#[async_trait]
impl Handler<Spawn> for IndexingPipeline {
    type Reply = ();

    async fn handle(
        &mut self,
        spawn: Spawn,
        ctx: &ActorContext<Self>,
    ) -> Result<(), ActorExitStatus> {
        if self.handles.is_some() {
            return Ok(());
        }
        self.previous_generations_statistics.num_spawn_attempts = 1 + spawn.retry_count;
        if let Err(spawn_error) = self.spawn_pipeline(ctx).await {
            if let Some(MetastoreError::IndexDoesNotExist { .. }) =
                spawn_error.downcast_ref::<MetastoreError>()
            {
                info!(error = ?spawn_error, "Could not spawn pipeline, index might have been deleted.");
                return Err(ActorExitStatus::Success);
            }
            let retry_delay = Self::wait_duration_before_retry(spawn.retry_count);
            error!(error = ?spawn_error, retry_count = spawn.retry_count, retry_delay = ?retry_delay, "Error while spawning indexing pipeline, retrying after some time.");
            ctx.schedule_self_msg(
                retry_delay,
                Spawn {
                    retry_count: spawn.retry_count + 1,
                },
            )
            .await;
        }
        Ok(())
    }
}

pub struct IndexingPipelineParams {
    pub pipeline_id: IndexingPipelineId,
    pub doc_mapper: Arc<dyn DocMapper>,
    pub indexing_directory: IndexingDirectory,
    pub indexing_settings: IndexingSettings,
    pub source_config: SourceConfig,
    pub split_store_max_num_bytes: usize,
    pub split_store_max_num_splits: usize,
    pub metastore: Arc<dyn Metastore>,
    pub storage: Arc<dyn Storage>,
}

impl IndexingPipelineParams {
    #[allow(clippy::too_many_arguments)]
    pub async fn try_new(
        pipeline_id: IndexingPipelineId,
        index_metadata: IndexMetadata,
        source_config: SourceConfig,
        indexing_dir_path: PathBuf,
        split_store_max_num_bytes: usize,
        split_store_max_num_splits: usize,
        metastore: Arc<dyn Metastore>,
        storage: Arc<dyn Storage>,
    ) -> anyhow::Result<Self> {
        let doc_mapper = build_doc_mapper(
            &index_metadata.doc_mapping,
            &index_metadata.search_settings,
            &index_metadata.indexing_settings,
        )?;
        let indexing_directory_path = indexing_dir_path
            .join(&pipeline_id.index_id)
            .join(&pipeline_id.source_id);
        let indexing_directory = IndexingDirectory::create_in_dir(indexing_directory_path).await?;
        Ok(Self {
            pipeline_id,
            doc_mapper,
            indexing_directory,
            indexing_settings: index_metadata.indexing_settings,
            source_config,
            split_store_max_num_bytes,
            split_store_max_num_splits,
            metastore,
            storage,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use quickwit_actors::Universe;
    use quickwit_config::{IndexingSettings, SourceParams};
    use quickwit_doc_mapper::default_doc_mapper_for_test;
    use quickwit_metastore::{IndexMetadata, MetastoreError, MockMetastore};
    use quickwit_storage::RamStorage;

    use super::{IndexingPipeline, *};
    use crate::models::IndexingDirectory;

    #[test]
    fn test_wait_duration() {
        assert_eq!(
            IndexingPipeline::wait_duration_before_retry(0),
            Duration::from_secs(2)
        );
        assert_eq!(
            IndexingPipeline::wait_duration_before_retry(1),
            Duration::from_secs(4)
        );
        assert_eq!(
            IndexingPipeline::wait_duration_before_retry(2),
            Duration::from_secs(8)
        );
        assert_eq!(
            IndexingPipeline::wait_duration_before_retry(3),
            Duration::from_secs(16)
        );
        assert_eq!(
            IndexingPipeline::wait_duration_before_retry(8),
            Duration::from_secs(512)
        );
        assert_eq!(
            IndexingPipeline::wait_duration_before_retry(9),
            MAX_RETRY_DELAY
        );
    }

    async fn test_indexing_pipeline_num_fails_before_success(
        mut num_fails: usize,
    ) -> anyhow::Result<bool> {
        let mut metastore = MockMetastore::default();
        metastore
            .expect_index_metadata()
            .withf(|index_id| index_id == "test-index")
            .returning(move |_| {
                if num_fails == 0 {
                    let index_metadata =
                        IndexMetadata::for_test("test-index", "ram:///indexes/test-index");
                    return Ok(index_metadata);
                }
                num_fails -= 1;
                Err(MetastoreError::ConnectionError {
                    message: "MetastoreError Alarm".to_string(),
                })
            });
        metastore
            .expect_list_splits()
            .returning(|_, _, _, _| Ok(Vec::new()));
        metastore
            .expect_mark_splits_for_deletion()
            .returning(|_, _| Ok(()));
        metastore
            .expect_stage_split()
            .withf(|index_id, _metadata| -> bool { index_id == "test-index" })
            .times(1)
            .returning(|_, _| Ok(()));
        metastore
            .expect_publish_splits()
            .withf(
                |index_id, splits, replaced_splits, checkpoint_delta_opt| -> bool {
                    let checkpoint_delta = checkpoint_delta_opt.as_ref().unwrap();
                    index_id == "test-index"
                        && checkpoint_delta.source_id == "test-source"
                        && splits.len() == 1
                        && replaced_splits.is_empty()
                        && format!("{:?}", checkpoint_delta.source_delta)
                            .ends_with(":(00000000000000000000..00000000000000001030])")
                },
            )
            .times(1)
            .returning(|_, _, _, _| Ok(()));
        let universe = Universe::new();
        let pipeline_id = IndexingPipelineId {
            index_id: "test-index".to_string(),
            source_id: "test-source".to_string(),
            node_id: "test-node".to_string(),
            pipeline_ord: 0,
        };
        let source_config = SourceConfig {
            source_id: "test-source".to_string(),
            num_pipelines: 1,
            source_params: SourceParams::file(PathBuf::from("data/test_corpus.json")),
        };
        let pipeline_params = IndexingPipelineParams {
            pipeline_id,
            doc_mapper: Arc::new(default_doc_mapper_for_test()),
            source_config,
            indexing_directory: IndexingDirectory::for_test().await?,
            indexing_settings: IndexingSettings::for_test(),
            split_store_max_num_bytes: 10_000_000,
            split_store_max_num_splits: 100,
            metastore: Arc::new(metastore),
            storage: Arc::new(RamStorage::default()),
        };
        let pipeline = IndexingPipeline::new(pipeline_params);
        let (_pipeline_mailbox, pipeline_handler) = universe.spawn_actor(pipeline).spawn();
        let (pipeline_exit_status, pipeline_statistics) = pipeline_handler.join().await;
        assert_eq!(pipeline_statistics.generation, 1);
        assert_eq!(pipeline_statistics.num_spawn_attempts, 1 + num_fails);
        Ok(pipeline_exit_status.is_success())
    }

    #[tokio::test]
    async fn test_indexing_pipeline_retry_1() -> anyhow::Result<()> {
        test_indexing_pipeline_num_fails_before_success(0).await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_indexing_pipeline_retry_2() -> anyhow::Result<()> {
        test_indexing_pipeline_num_fails_before_success(2).await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_indexing_pipeline_retry_3() -> anyhow::Result<()> {
        test_indexing_pipeline_num_fails_before_success(3).await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_indexing_pipeline_simple() -> anyhow::Result<()> {
        let mut metastore = MockMetastore::default();
        metastore
            .expect_index_metadata()
            .withf(|index_id| index_id == "test-index")
            .returning(|_| {
                Ok(IndexMetadata::for_test(
                    "test-index",
                    "ram:///indexes/test-index",
                ))
            });
        metastore
            .expect_list_splits()
            .times(3)
            .returning(|_, _, _, _| Ok(Vec::new()));
        metastore
            .expect_mark_splits_for_deletion()
            .times(1)
            .returning(|_, _| Ok(()));
        metastore
            .expect_stage_split()
            .withf(|index_id, _metadata| index_id == "test-index")
            .times(1)
            .returning(|_, _| Ok(()));
        metastore
            .expect_publish_splits()
            .withf(
                |index_id, splits, replaced_split_ids, checkpoint_delta_opt| -> bool {
                    let checkpoint_delta = checkpoint_delta_opt.as_ref().unwrap();
                    index_id == "test-index"
                        && splits.len() == 1
                        && replaced_split_ids.is_empty()
                        && checkpoint_delta.source_id == "test-source"
                        && format!("{:?}", checkpoint_delta.source_delta)
                            .ends_with(":(00000000000000000000..00000000000000001030])")
                },
            )
            .times(1)
            .returning(|_, _, _, _| Ok(()));
        let universe = Universe::new();
        let pipeline_id = IndexingPipelineId {
            index_id: "test-index".to_string(),
            source_id: "test-source".to_string(),
            node_id: "test-node".to_string(),
            pipeline_ord: 0,
        };
        let source_config = SourceConfig {
            source_id: "test-source".to_string(),
            num_pipelines: 1,
            source_params: SourceParams::file(PathBuf::from("data/test_corpus.json")),
        };
        let pipeline_params = IndexingPipelineParams {
            pipeline_id,
            doc_mapper: Arc::new(default_doc_mapper_for_test()),
            source_config,
            indexing_directory: IndexingDirectory::for_test().await?,
            indexing_settings: IndexingSettings::for_test(),
            split_store_max_num_bytes: 10_000_000,
            split_store_max_num_splits: 100,
            metastore: Arc::new(metastore),
            storage: Arc::new(RamStorage::default()),
        };
        let pipeline = IndexingPipeline::new(pipeline_params);
        let (_pipeline_mailbox, pipeline_handler) = universe.spawn_actor(pipeline).spawn();
        let (pipeline_exit_status, pipeline_statistics) = pipeline_handler.join().await;
        assert!(pipeline_exit_status.is_success());
        assert_eq!(pipeline_statistics.generation, 1);
        assert_eq!(pipeline_statistics.num_spawn_attempts, 1);
        assert_eq!(pipeline_statistics.num_published_splits, 1);
        Ok(())
    }
}
