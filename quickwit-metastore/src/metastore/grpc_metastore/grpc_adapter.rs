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

use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use itertools::Itertools;
use quickwit_common::extract_time_range;
use quickwit_doc_mapper::tag_pruning::TagFilterAst;
use quickwit_proto::metastore_api::metastore_api_service_server::{self as grpc};
use quickwit_proto::metastore_api::{
    AddSourceRequest, CreateIndexRequest, CreateIndexResponse, DeleteIndexRequest,
    DeleteIndexResponse, DeleteSourceRequest, DeleteSplitsRequest, IndexMetadataRequest,
    IndexMetadataResponse, ListAllSplitsRequest, ListIndexesMetadatasRequest,
    ListIndexesMetadatasResponse, ListSplitsRequest, ListSplitsResponse,
    MarkSplitsForDeletionRequest, PublishSplitsRequest, ResetSourceCheckpointRequest,
    SourceResponse, SplitResponse, StageSplitRequest,
};
use quickwit_proto::tonic;

use crate::{IndexMetadata, Metastore, MetastoreError, SplitState};

#[allow(missing_docs)]
#[derive(Clone)]
pub struct GrpcMetastoreAdapter(Arc<dyn Metastore>);

impl From<Arc<dyn Metastore>> for GrpcMetastoreAdapter {
    fn from(metastore: Arc<dyn Metastore>) -> Self {
        Self(metastore)
    }
}

#[async_trait]
impl grpc::MetastoreApiService for GrpcMetastoreAdapter {
    async fn create_index(
        &self,
        request: tonic::Request<CreateIndexRequest>,
    ) -> Result<tonic::Response<CreateIndexResponse>, tonic::Status> {
        let create_index_request = request.into_inner();
        let index_metadata = serde_json::from_str::<IndexMetadata>(
            &create_index_request.index_metadata_serialized_json,
        )
        .map_err(|error| MetastoreError::JsonDeserializeError {
            name: "IndexMetadata".to_string(),
            message: error.to_string(),
        })?;
        let create_index_reply = self
            .0
            .create_index(index_metadata)
            .await
            .map(|_| CreateIndexResponse {})?;
        Ok(tonic::Response::new(create_index_reply))
    }

    async fn index_metadata(
        &self,
        request: tonic::Request<IndexMetadataRequest>,
    ) -> Result<tonic::Response<IndexMetadataResponse>, tonic::Status> {
        let index_metadata_request = request.into_inner();
        let index_metadata = self
            .0
            .index_metadata(&index_metadata_request.index_id)
            .await?;
        let index_metadata_reply = serde_json::to_string(&index_metadata)
            .map(|index_metadata_serialized_json| IndexMetadataResponse {
                index_metadata_serialized_json,
            })
            .map_err(|error| MetastoreError::JsonSerializeError {
                name: "IndexMetadata".to_string(),
                message: error.to_string(),
            })?;
        Ok(tonic::Response::new(index_metadata_reply))
    }

    async fn list_indexes_metadatas(
        &self,
        _: tonic::Request<ListIndexesMetadatasRequest>,
    ) -> Result<tonic::Response<ListIndexesMetadatasResponse>, tonic::Status> {
        let indexes_metadatas = self.0.list_indexes_metadatas().await?;
        let list_indexes_metadatas_reply = serde_json::to_string(&indexes_metadatas)
            .map(
                |indexes_metadatas_serialized_json| ListIndexesMetadatasResponse {
                    indexes_metadatas_serialized_json,
                },
            )
            .map_err(|error| MetastoreError::JsonSerializeError {
                name: "Vec<IndexMetadata>".to_string(),
                message: error.to_string(),
            })?;
        Ok(tonic::Response::new(list_indexes_metadatas_reply))
    }

    async fn delete_index(
        &self,
        request: tonic::Request<DeleteIndexRequest>,
    ) -> Result<tonic::Response<DeleteIndexResponse>, tonic::Status> {
        let delete_request = request.into_inner();
        let delete_reply = self
            .0
            .delete_index(&delete_request.index_id)
            .await
            .map(|_| DeleteIndexResponse {})?;
        Ok(tonic::Response::new(delete_reply))
    }

    async fn list_all_splits(
        &self,
        request: tonic::Request<ListAllSplitsRequest>,
    ) -> Result<tonic::Response<ListSplitsResponse>, tonic::Status> {
        let list_all_splits_request = request.into_inner();
        let splits = self
            .0
            .list_all_splits(&list_all_splits_request.index_id)
            .await?;
        let list_all_splits_reply = serde_json::to_string(&splits)
            .map(|splits_serialized_json| ListSplitsResponse {
                splits_serialized_json,
            })
            .map_err(|error| MetastoreError::JsonSerializeError {
                name: "Vec<Split>".to_string(),
                message: error.to_string(),
            })?;
        Ok(tonic::Response::new(list_all_splits_reply))
    }

    async fn list_splits(
        &self,
        request: tonic::Request<ListSplitsRequest>,
    ) -> Result<tonic::Response<ListSplitsResponse>, tonic::Status> {
        let list_splits_request = request.into_inner();
        let split_state: SplitState = SplitState::from_str(&list_splits_request.split_state)
            .map_err(|error| MetastoreError::JsonDeserializeError {
                name: "SplitState".to_string(),
                message: error,
            })?;
        let time_range = extract_time_range(
            list_splits_request.time_range_start,
            list_splits_request.time_range_end,
        );
        let tags = list_splits_request
            .tags_serialized_json
            .map(|tags| {
                serde_json::from_str::<TagFilterAst>(&tags).map_err(|error| {
                    MetastoreError::JsonDeserializeError {
                        name: "TagFilterAst".to_string(),
                        message: error.to_string(),
                    }
                })
            })
            .transpose()?;
        let splits = self
            .0
            .list_splits(&list_splits_request.index_id, split_state, time_range, tags)
            .await?;
        let list_splits_reply = serde_json::to_string(&splits)
            .map(|splits_serialized_json| ListSplitsResponse {
                splits_serialized_json,
            })
            .map_err(|error| MetastoreError::JsonSerializeError {
                name: "Vec<Split>".to_string(),
                message: error.to_string(),
            })?;
        Ok(tonic::Response::new(list_splits_reply))
    }

    async fn stage_split(
        &self,
        request: tonic::Request<StageSplitRequest>,
    ) -> Result<tonic::Response<SplitResponse>, tonic::Status> {
        let stage_split_request = request.into_inner();
        let split_metadata = serde_json::from_str(
            &stage_split_request.split_metadata_serialized_json,
        )
        .map_err(|error| MetastoreError::JsonDeserializeError {
            name: "SplitMetadata".to_string(),
            message: error.to_string(),
        })?;
        let stage_split_reply = self
            .0
            .stage_split(&stage_split_request.index_id, split_metadata)
            .await
            .map(|_| SplitResponse {})?;
        Ok(tonic::Response::new(stage_split_reply))
    }

    async fn publish_splits(
        &self,
        request: tonic::Request<PublishSplitsRequest>,
    ) -> Result<tonic::Response<SplitResponse>, tonic::Status> {
        let publish_request = request.into_inner();
        let split_ids = publish_request
            .split_ids
            .iter()
            .map(|split_id| split_id.as_str())
            .collect_vec();
        let replaced_split_ids = publish_request
            .replaced_split_ids
            .iter()
            .map(|split_id| split_id.as_str())
            .collect_vec();
        let checkpoint_delta_opt = publish_request
            .index_checkpoint_delta_serialized_json
            .map(|json| serde_json::from_str(&json))
            .transpose()
            .map_err(|error| MetastoreError::JsonDeserializeError {
                name: "IndexCheckpointDelta".to_string(),
                message: error.to_string(),
            })?;
        let publish_splits_reply = self
            .0
            .publish_splits(
                &publish_request.index_id,
                &split_ids,
                &replaced_split_ids,
                checkpoint_delta_opt,
            )
            .await
            .map(|_| SplitResponse {})?;
        Ok(tonic::Response::new(publish_splits_reply))
    }

    async fn mark_splits_for_deletion(
        &self,
        request: tonic::Request<MarkSplitsForDeletionRequest>,
    ) -> Result<tonic::Response<SplitResponse>, tonic::Status> {
        let mark_splits_for_deletion_request = request.into_inner();
        let split_ids = mark_splits_for_deletion_request
            .split_ids
            .iter()
            .map(|split_id| split_id.as_str())
            .collect_vec();
        let mark_splits_for_deletion_reply = self
            .0
            .mark_splits_for_deletion(&mark_splits_for_deletion_request.index_id, &split_ids)
            .await
            .map(|_| SplitResponse {})?;
        Ok(tonic::Response::new(mark_splits_for_deletion_reply))
    }

    async fn delete_splits(
        &self,
        request: tonic::Request<DeleteSplitsRequest>,
    ) -> Result<tonic::Response<SplitResponse>, tonic::Status> {
        let delete_splits_request = request.into_inner();
        let split_ids = delete_splits_request
            .split_ids
            .iter()
            .map(|split_id| split_id.as_str())
            .collect_vec();
        let delete_splits_reply = self
            .0
            .delete_splits(&delete_splits_request.index_id, &split_ids)
            .await
            .map(|_| SplitResponse {})?;
        Ok(tonic::Response::new(delete_splits_reply))
    }

    async fn add_source(
        &self,
        request: tonic::Request<AddSourceRequest>,
    ) -> Result<tonic::Response<SourceResponse>, tonic::Status> {
        let add_source_request = request.into_inner();
        let source_config = serde_json::from_str(&add_source_request.source_config_serialized_json)
            .map_err(|error| MetastoreError::JsonDeserializeError {
                name: "SourceConfig".to_string(),
                message: error.to_string(),
            })?;
        let add_source_reply = self
            .0
            .add_source(&add_source_request.index_id, source_config)
            .await
            .map(|_| SourceResponse {})?;
        Ok(tonic::Response::new(add_source_reply))
    }

    async fn delete_source(
        &self,
        request: tonic::Request<DeleteSourceRequest>,
    ) -> Result<tonic::Response<SourceResponse>, tonic::Status> {
        let delete_source_request = request.into_inner();
        let delete_source_reply = self
            .0
            .delete_source(
                &delete_source_request.index_id,
                &delete_source_request.source_id,
            )
            .await
            .map(|_| SourceResponse {})?;
        Ok(tonic::Response::new(delete_source_reply))
    }

    async fn reset_source_checkpoint(
        &self,
        request: tonic::Request<ResetSourceCheckpointRequest>,
    ) -> Result<tonic::Response<SourceResponse>, tonic::Status> {
        let request = request.into_inner();
        let reply = self
            .0
            .reset_source_checkpoint(&request.index_id, &request.source_id)
            .await
            .map(|_| SourceResponse {})?;
        Ok(tonic::Response::new(reply))
    }
}
