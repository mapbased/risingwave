// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashSet;
use std::iter::Map;
use std::time::Duration;

use futures::future::try_join_all;
use log::{debug, error};
use risingwave_common::error::{ErrorCode, Result, RwError, ToRwResult};
use risingwave_pb::common::ActorInfo;
use risingwave_pb::data::Epoch as ProstEpoch;
use risingwave_pb::stream_service::inject_barrier_response::FinishedCreateMview;
use risingwave_pb::stream_service::{
    BroadcastActorInfoTableRequest, BuildActorsRequest, ForceStopActorsRequest, SyncSourcesRequest,
    UpdateActorsRequest,
};
use tokio_retry::strategy::{jitter, ExponentialBackoff};
use uuid::Uuid;

use crate::barrier::command::CommandContext;
use crate::barrier::info::BarrierActorInfo;
use crate::barrier::{Command, GlobalBarrierManager};
use crate::manager::Epoch;
use crate::model::ActorId;
use crate::storage::MetaStore;

pub type RecoveryResult = (Epoch, HashSet<ActorId>, Vec<FinishedCreateMview>);

impl<S> GlobalBarrierManager<S>
where
    S: MetaStore,
{
    // Retry base interval in milliseconds.
    const RECOVERY_RETRY_BASE_INTERVAL: u64 = 100;
    // Retry max interval.
    const RECOVERY_RETRY_MAX_INTERVAL: Duration = Duration::from_secs(10);

    #[inline(always)]
    /// Initialize a retry strategy for operation in recovery.
    fn get_retry_strategy() -> Map<ExponentialBackoff, fn(Duration) -> Duration> {
        ExponentialBackoff::from_millis(Self::RECOVERY_RETRY_BASE_INTERVAL)
            .max_delay(Self::RECOVERY_RETRY_MAX_INTERVAL)
            .map(jitter)
    }

    /// Recovery the whole cluster from the latest epoch.
    pub(crate) async fn recovery(
        &self,
        prev_epoch: u64,
        prev_command: Option<Command>,
    ) -> RecoveryResult {
        // Abort buffered schedules, they might be dirty already.
        self.scheduled_barriers.abort().await;

        // clean up the previous command dirty data.
        if let Some(prev_command) = prev_command {
            self.clean_up(prev_command).await;
        }

        debug!("recovery start!");
        let retry_strategy = Self::get_retry_strategy();
        let (new_epoch, responses) = tokio_retry::Retry::spawn(retry_strategy, || async {
            let info = self.resolve_actor_info(None).await;
            let mut new_epoch = self.env.epoch_generator().generate();

            // Reset all compute nodes, stop and drop existing actors.
            self.reset_compute_nodes(&info, prev_epoch, new_epoch.into_inner())
                .await;

            // Refresh sources in local source manger of compute node.
            if let Err(err) = self.sync_sources(&info).await {
                error!("sync_sources failed: {}", err);
                return Err(err);
            }

            // update and build all actors.
            if let Err(err) = self.update_actors(&info).await {
                error!("update_actors failed: {}", err);
                return Err(err);
            }
            if let Err(err) = self.build_actors(&info).await {
                error!("build_actors failed: {}", err);
                return Err(err);
            }

            let prev_epoch = new_epoch.into_inner();
            new_epoch = self.env.epoch_generator().generate();
            // checkpoint, used as init barrier to initialize all executors.
            let command_ctx = CommandContext::new(
                self.fragment_manager.clone(),
                self.env.stream_clients_ref(),
                &info,
                prev_epoch,
                new_epoch.into_inner(),
                Command::checkpoint(),
            );

            match self.inject_barrier(&command_ctx).await {
                Ok(response) => {
                    if let Err(err) = command_ctx.post_collect().await {
                        error!("post_collect failed: {}", err);
                        return Err(err);
                    }
                    Ok((new_epoch, response))
                }
                Err(err) => {
                    error!("inject_barrier failed: {}", err);
                    Err(err)
                }
            }
        })
        .await
        .expect("Retry until recovery success.");
        debug!("recovery success");

        return (
            new_epoch,
            self.fragment_manager.all_chain_actor_ids().await,
            responses
                .into_iter()
                .flat_map(|r| r.finished_create_mviews)
                .collect(),
        );
    }

    /// Clean up previous command dirty data. Currently, we only need to handle table fragments info
    /// for `CreateMaterializedView`. For `DropMaterializedView`, since we already response fail to
    /// frontend and the actors will be rebuild by follow recovery process, it's okay to retain
    /// it.
    async fn clean_up(&self, prev_command: Command) {
        if let Some(table_id) = prev_command.creating_table_id() {
            let retry_strategy = Self::get_retry_strategy();
            tokio_retry::Retry::spawn(retry_strategy, || async {
                self.fragment_manager.drop_table_fragments(&table_id).await
            })
            .await
            .expect("Retry clean up until success");
        }
    }

    /// Sync all sources in compute nodes, the local source manager in compute nodes may be dirty
    /// already.
    async fn sync_sources(&self, info: &BarrierActorInfo) -> Result<()> {
        // Attention, using catalog v2 here, it's not compatible with Java frontend.
        let catalog_guard = self.catalog_manager.get_catalog_core_guard().await;
        let sources = catalog_guard.list_sources().await?;

        let futures = info.node_map.iter().map(|(_, node)| {
            let request = SyncSourcesRequest {
                sources: sources.clone(),
            };
            async move {
                let client = &self.env.stream_clients().get(node).await?;
                client
                    .to_owned()
                    .sync_sources(request)
                    .await
                    .to_rw_result()?;

                Ok::<_, RwError>(())
            }
        });

        try_join_all(futures).await?;

        Ok(())
    }

    /// Update all actors in compute nodes.
    async fn update_actors(&self, info: &BarrierActorInfo) -> Result<()> {
        let mut actor_infos = vec![];
        for (node_id, actors) in &info.actor_map {
            let host = info
                .node_map
                .get(node_id)
                .ok_or_else(|| {
                    RwError::from(ErrorCode::InternalError(
                        "worker evicted, wait for online.".to_string(),
                    ))
                })?
                .host
                .clone();
            actor_infos.extend(actors.iter().map(|&actor_id| ActorInfo {
                actor_id,
                host: host.clone(),
            }));
        }

        let node_actors = self.fragment_manager.all_node_actors(false).await;
        for (node_id, actors) in &info.actor_map {
            let node = info.node_map.get(node_id).unwrap();
            let client = self.env.stream_clients().get(node).await?;

            client
                .to_owned()
                .broadcast_actor_info_table(BroadcastActorInfoTableRequest {
                    info: actor_infos.clone(),
                })
                .await
                .to_rw_result_with(|| format!("failed to connect to {}", node_id))?;

            let request_id = Uuid::new_v4().to_string();
            tracing::debug!(request_id = request_id.as_str(), actors = ?actors, "update actors");
            client
                .to_owned()
                .update_actors(UpdateActorsRequest {
                    request_id,
                    actors: node_actors.get(node_id).cloned().unwrap_or_default(),
                    ..Default::default()
                })
                .await
                .to_rw_result_with(|| format!("failed to connect to {}", node_id))?;
        }

        Ok(())
    }

    /// Build all actors in compute nodes.
    async fn build_actors(&self, info: &BarrierActorInfo) -> Result<()> {
        for (node_id, actors) in &info.actor_map {
            let node = info.node_map.get(node_id).unwrap();
            let client = self.env.stream_clients().get(node).await?;

            let request_id = Uuid::new_v4().to_string();
            tracing::debug!(request_id = request_id.as_str(), actors = ?actors, "build actors");
            client
                .to_owned()
                .build_actors(BuildActorsRequest {
                    request_id,
                    actor_id: actors.to_owned(),
                })
                .await
                .to_rw_result_with(|| format!("failed to connect to {}", node_id))?;
        }

        Ok(())
    }

    /// Reset all compute nodes by calling `force_stop_actors`.
    async fn reset_compute_nodes(&self, info: &BarrierActorInfo, prev_epoch: u64, new_epoch: u64) {
        let futures = info.node_map.iter().map(|(_, worker_node)| {
            let retry_strategy = Self::get_retry_strategy();

            async move {
                tokio_retry::Retry::spawn(retry_strategy, || async {
                    let client = self.env.stream_clients().get(worker_node).await?;
                    debug!("force stop actors: {}", worker_node.id);
                    client
                        .to_owned()
                        .force_stop_actors(ForceStopActorsRequest {
                            request_id: Uuid::new_v4().to_string(),
                            epoch: Some(ProstEpoch {
                                curr: new_epoch,
                                prev: prev_epoch,
                            }),
                        })
                        .await
                        .to_rw_result()
                })
                .await
                .expect("Force stop actors until success");

                Ok::<_, RwError>(())
            }
        });

        try_join_all(futures).await.unwrap();
        debug!("all compute nodes have been reset.");
    }
}