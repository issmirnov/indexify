use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    hash::{Hash, Hasher},
    net::SocketAddr,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::{Context, Poll},
};

use anyhow::{anyhow, Result};
use axum::{extract::State, routing::get};
use futures::StreamExt;
use hyper::StatusCode;
use indexify_internal_api as internal_api;
use indexify_proto::indexify_coordinator::{
    self,
    coordinator_service_server::CoordinatorService,
    CoordinatorCommand,
    CreateContentRequest,
    CreateContentResponse,
    CreateGcTasksRequest,
    CreateGcTasksResponse,
    CreateIndexRequest,
    CreateIndexResponse,
    ExtractionPolicyRequest,
    ExtractionPolicyResponse,
    GcTask,
    GcTaskAcknowledgement,
    GetAllSchemaRequest,
    GetAllSchemaResponse,
    GetAllTaskAssignmentRequest,
    GetContentMetadataRequest,
    GetContentTreeMetadataRequest,
    GetExtractorCoordinatesRequest,
    GetIndexRequest,
    GetIndexResponse,
    GetIngestionInfoRequest,
    GetIngestionInfoResponse,
    GetRaftMetricsSnapshotRequest,
    GetSchemaRequest,
    GetSchemaResponse,
    GetTaskRequest,
    GetTaskResponse,
    HeartbeatRequest,
    HeartbeatResponse,
    ListContentRequest,
    ListContentResponse,
    ListExtractionPoliciesRequest,
    ListExtractionPoliciesResponse,
    ListExtractorsRequest,
    ListExtractorsResponse,
    ListIndexesRequest,
    ListIndexesResponse,
    ListStateChangesRequest,
    ListTasksRequest,
    ListTasksResponse,
    RaftMetricsSnapshotResponse,
    RegisterExecutorRequest,
    RegisterExecutorResponse,
    RegisterIngestionServerRequest,
    RegisterIngestionServerResponse,
    RemoveIngestionServerRequest,
    RemoveIngestionServerResponse,
    TaskAssignments,
    TombstoneContentRequest,
    TombstoneContentResponse,
    Uint64List,
    UpdateTaskRequest,
    UpdateTaskResponse,
};
use internal_api::StateChange;
use opentelemetry::{
    global,
    metrics::{Histogram, UpDownCounter},
    propagation::Extractor,
    KeyValue,
};
use prometheus::Encoder;
use tokio::{
    select,
    signal,
    sync::{
        mpsc,
        watch::{self, Receiver, Sender},
    },
    task::JoinHandle,
};
use tokio_stream::{wrappers::ReceiverStream, Stream};
use tonic::{body::BoxBody, Request, Response, Status, Streaming};
use tower::{Layer, Service, ServiceBuilder};
use tracing::{error, info, Instrument};

use crate::{
    api::IndexifyAPIError,
    coordinator::Coordinator,
    coordinator_client::CoordinatorClient,
    garbage_collector::GarbageCollector,
    server_config::ServerConfig,
    state,
    tonic_streamer::DropReceiver,
    utils::timestamp_secs,
};

type HBResponseStream = Pin<Box<dyn Stream<Item = Result<HeartbeatResponse, Status>> + Send>>;
type GCTasksResponseStream =
    Pin<Box<dyn tokio_stream::Stream<Item = Result<CoordinatorCommand, Status>> + Send + Sync>>;

pub struct CoordinatorServiceServer {
    coordinator: Arc<Coordinator>,
    shutdown_rx: Receiver<()>,
}

struct MetadataMap<'a>(&'a reqwest::header::HeaderMap);

impl<'a> Extractor for MetadataMap<'a> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|metadata| metadata.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|key| key.as_str()).collect::<Vec<_>>()
    }
}

#[tonic::async_trait]
impl CoordinatorService for CoordinatorServiceServer {
    type GCTasksStreamStream = GCTasksResponseStream;
    type HeartbeatStream = HBResponseStream;

    async fn create_content(
        &self,
        request: tonic::Request<CreateContentRequest>,
    ) -> Result<tonic::Response<CreateContentResponse>, tonic::Status> {
        let content_meta = request
            .into_inner()
            .content
            .ok_or(tonic::Status::aborted("content is missing"))?;
        let id = content_meta.id.clone();
        let content_list = vec![content_meta];
        let _ = self
            .coordinator
            .create_content_metadata(content_list)
            .await
            .map_err(|e| tonic::Status::aborted(e.to_string()))?;
        Ok(tonic::Response::new(CreateContentResponse { id }))
    }

    async fn tombstone_content(
        &self,
        request: tonic::Request<TombstoneContentRequest>,
    ) -> Result<tonic::Response<TombstoneContentResponse>, tonic::Status> {
        let req = request.into_inner();
        let namespace = req.namespace;
        let content_ids = req.content_ids;
        self.coordinator
            .tombstone_content_metadatas(&namespace, &content_ids)
            .await
            .map_err(|e| tonic::Status::aborted(e.to_string()))?;
        Ok(tonic::Response::new(TombstoneContentResponse {}))
    }

    async fn list_content(
        &self,
        request: tonic::Request<ListContentRequest>,
    ) -> Result<tonic::Response<ListContentResponse>, tonic::Status> {
        let req = request.into_inner();
        let content_list = self
            .coordinator
            .list_content(&req.namespace, &req.source, &req.parent_id, &req.labels_eq)
            .await
            .map_err(|e| tonic::Status::aborted(e.to_string()))?;
        Ok(tonic::Response::new(ListContentResponse {
            content_list: content_list
                .into_iter()
                .map(|c| c.into())
                .collect::<Vec<indexify_coordinator::ContentMetadata>>(),
        }))
    }

    async fn create_extraction_policy(
        &self,
        request: tonic::Request<ExtractionPolicyRequest>,
    ) -> Result<tonic::Response<ExtractionPolicyResponse>, tonic::Status> {
        let request = request.into_inner();
        let mut s = DefaultHasher::new();
        request.namespace.hash(&mut s);
        request.name.hash(&mut s);
        let id = s.finish().to_string();
        let input_params = serde_json::from_str(&request.input_params)
            .map_err(|e| tonic::Status::aborted(format!("unable to parse input_params: {}", e)))?;

        let extractor = self
            .coordinator
            .get_extractor(&request.extractor)
            .await
            .map_err(|e| tonic::Status::aborted(e.to_string()))?;
        let mut index_name_table_mapping = HashMap::new();
        let mut output_index_name_mapping = HashMap::new();

        //  TODO: Just create an output to table mapping here directly instead of 2
        // separate mappings
        for output_name in extractor.outputs.keys() {
            let index_name = format!("{}.{}", request.name, output_name);
            let index_table_name =
                format!("{}.{}.{}", request.namespace, request.name, output_name);
            index_name_table_mapping.insert(index_name.clone(), index_table_name.clone());
            output_index_name_mapping.insert(output_name.clone(), index_name.clone());
        }

        let extraction_policy = internal_api::ExtractionPolicy {
            id,
            extractor: request.extractor,
            name: request.name,
            namespace: request.namespace,
            filters: request.filters,
            input_params,
            output_index_name_mapping: output_index_name_mapping.clone(),
            index_name_table_mapping: index_name_table_mapping.clone(),
            content_source: request.content_source,
        };
        let _ = self
            .coordinator
            .create_policy(extraction_policy.clone(), extractor.clone())
            .await
            .map_err(|e| tonic::Status::aborted(e.to_string()))?;
        Ok(tonic::Response::new(ExtractionPolicyResponse {
            created_at: timestamp_secs() as i64,
            extractor: Some(extractor.into()),
            extraction_policy: Some(extraction_policy.into()),
            index_name_table_mapping,
            output_index_name_mapping,
        }))
    }

    async fn list_extraction_policies(
        &self,
        request: tonic::Request<ListExtractionPoliciesRequest>,
    ) -> Result<tonic::Response<ListExtractionPoliciesResponse>, tonic::Status> {
        let request = request.into_inner();
        let extraction_policies = self
            .coordinator
            .list_policies(&request.namespace)
            .await
            .map_err(|e| tonic::Status::aborted(e.to_string()))?;
        let policies = extraction_policies
            .into_iter()
            .map(|b| b.into())
            .collect::<Vec<indexify_coordinator::ExtractionPolicy>>();

        Ok(tonic::Response::new(ListExtractionPoliciesResponse {
            policies,
        }))
    }

    async fn create_ns(
        &self,
        request: tonic::Request<indexify_coordinator::CreateNamespaceRequest>,
    ) -> Result<tonic::Response<indexify_coordinator::CreateNamespaceResponse>, tonic::Status> {
        let request = request.into_inner();
        self.coordinator
            .create_namespace(&request.name)
            .await
            .map_err(|e| tonic::Status::aborted(e.to_string()))?;
        Ok(tonic::Response::new(
            indexify_coordinator::CreateNamespaceResponse {
                name: request.name,
                created_at: 0,
            },
        ))
    }

    async fn list_ns(
        &self,
        _request: tonic::Request<indexify_coordinator::ListNamespaceRequest>,
    ) -> Result<tonic::Response<indexify_coordinator::ListNamespaceResponse>, tonic::Status> {
        let namespaces = self
            .coordinator
            .list_namespaces()
            .await
            .map_err(|e| tonic::Status::aborted(e.to_string()))?;
        let namespaces = namespaces
            .into_iter()
            .map(|r| r.into())
            .collect::<Vec<indexify_coordinator::Namespace>>();
        Ok(tonic::Response::new(
            indexify_coordinator::ListNamespaceResponse { namespaces },
        ))
    }

    async fn get_ns(
        &self,
        request: tonic::Request<indexify_coordinator::GetNamespaceRequest>,
    ) -> Result<tonic::Response<indexify_coordinator::GetNamespaceResponse>, tonic::Status> {
        let namespace = request.into_inner().name;
        let namespace = self
            .coordinator
            .get_namespace(&namespace)
            .await
            .map_err(|e| tonic::Status::aborted(e.to_string()))?;

        Ok(tonic::Response::new(
            indexify_coordinator::GetNamespaceResponse {
                namespace: namespace.map(|n| n.into()),
            },
        ))
    }

    async fn list_extractors(
        &self,
        _request: tonic::Request<ListExtractorsRequest>,
    ) -> Result<tonic::Response<ListExtractorsResponse>, tonic::Status> {
        let extractors = self
            .coordinator
            .list_extractors()
            .await
            .map_err(|e| tonic::Status::aborted(e.to_string()))?;
        let extractors = extractors
            .into_iter()
            .map(|e| e.into())
            .collect::<Vec<indexify_coordinator::Extractor>>();
        Ok(tonic::Response::new(ListExtractorsResponse { extractors }))
    }

    // TODO: edwin
    async fn register_executor(
        &self,
        request: tonic::Request<RegisterExecutorRequest>,
    ) -> Result<tonic::Response<RegisterExecutorResponse>, tonic::Status> {
        let request = request.into_inner();

        let extractors = request
            .extractors
            .into_iter()
            .map(|e| e.into())
            .collect::<Vec<internal_api::ExtractorDescription>>();

        let _resp = self
            .coordinator
            .register_executor(&request.addr, &request.executor_id, extractors.into())
            .await
            .map_err(|e| tonic::Status::aborted(e.to_string()))?;

        Ok(tonic::Response::new(RegisterExecutorResponse {
            executor_id: request.executor_id,
        }))
    }

    async fn register_ingestion_server(
        &self,
        request: tonic::Request<RegisterIngestionServerRequest>,
    ) -> Result<tonic::Response<RegisterIngestionServerResponse>, tonic::Status> {
        let request = request.into_inner();
        self.coordinator
            .register_ingestion_server(&request.ingestion_server_id)
            .await
            .map_err(|e| tonic::Status::aborted(e.to_string()))?;

        Ok(tonic::Response::new(RegisterIngestionServerResponse {}))
    }

    async fn remove_ingestion_server(
        &self,
        request: tonic::Request<RemoveIngestionServerRequest>,
    ) -> Result<tonic::Response<RemoveIngestionServerResponse>, tonic::Status> {
        let request = request.into_inner();
        self.coordinator
            .remove_ingestion_server(&request.ingestion_server_id)
            .await
            .map_err(|e| tonic::Status::aborted(e.to_string()))?;

        Ok(tonic::Response::new(RemoveIngestionServerResponse {}))
    }

    async fn create_gc_tasks(
        &self,
        request: tonic::Request<CreateGcTasksRequest>,
    ) -> Result<tonic::Response<CreateGcTasksResponse>, tonic::Status> {
        let request = request.into_inner();
        let state_change = request.state_change.ok_or_else(|| {
            tonic::Status::aborted("missing state change in create gc tasks request")
        })?;
        let state_change: indexify_internal_api::StateChange =
            state_change.try_into().map_err(|e| {
                tonic::Status::aborted(format!(
                    "unable to convert state change to internal api: {}",
                    e
                ))
            })?;
        self.coordinator
            .create_gc_tasks(&state_change)
            .await
            .map_err(|e| tonic::Status::aborted(e.to_string()))?;
        Ok(tonic::Response::new(CreateGcTasksResponse {}))
    }

    async fn gc_tasks_stream(
        &self,
        request: tonic::Request<Streaming<GcTaskAcknowledgement>>,
    ) -> Result<tonic::Response<Self::GCTasksStreamStream>, Status> {
        let mut gc_task_allocation_event_rx = self.coordinator.subscribe_to_gc_events().await;
        let (tx, rx) = mpsc::channel(100);

        let mut inbound = request.into_inner();
        let coordinator_clone = self.coordinator.clone();
        let mut shutdown_rx = self.shutdown_rx.clone();

        let mut ingestion_server_id: Option<String> = None;

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        info!("Shutdown signal received, terminating gc_tasks_stream.");
                        break;
                    }
                    task_ack = inbound.next() => {
                        match task_ack {
                            Some(Ok(task_ack)) => {
                                //  check for heartbeat
                                if task_ack.task_id.is_empty() {
                                    ingestion_server_id.replace(task_ack.ingestion_server_id);
                                    if let Err(e) = coordinator_clone.register_ingestion_server(ingestion_server_id.as_ref().unwrap()).await {
                                        tracing::error!("Error registering ingestion server: {}", e);
                                    }
                                    continue;
                                }

                                tracing::info!(
                                    "Received gc task acknowledgement {:?}, marking the gc task as complete",
                                    task_ack
                                );
                                if let Err(e) = coordinator_clone
                                .update_gc_task(&task_ack.task_id, task_ack.completed.into())
                                .await
                                {
                                    tracing::error!(
                                        "Error updating GC task with id {}: {}",
                                        task_ack.task_id,
                                        e
                                    );
                                }
                            }
                            Some(Err(e)) => {
                                tracing::error!("Stream error, likely disconnection: {}", e);
                                break;
                            }
                            None => {
                                tracing::info!("GC tasks stream ended, client disconnected.");
                                break;
                            }
                        }
                    }
                    task_allocation_event = gc_task_allocation_event_rx.recv() => {
                        match task_allocation_event {
                            Ok(task_allocation) => {
                                let task = task_allocation;
                                if let Some(ref server_id) = ingestion_server_id {
                                    if task.assigned_to.is_some() && &task.assigned_to.clone().unwrap() == server_id {
                                        let serialized_task: GcTask = task.into();
                                        let command = CoordinatorCommand {
                                            gc_task: Some(serialized_task)
                                        };
                                        tx.send(command).await.unwrap();
                                    }
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                tracing::error!("Skipped {} messages due to lagging", n);
                                //  TODO: How should skipped gc tasks be handled?
                            }
                            Err(e) => {
                                tracing::error!("Error receiving gc task allocation event: {}", e);
                            }
                        }
                    }
                }
            }

            //  Notify the garbage collector that the ingestion server has disconnected
            if let Some(server_id) = ingestion_server_id {
                if let Err(e) = coordinator_clone.remove_ingestion_server(&server_id).await {
                    tracing::error!("Error removing ingestion server: {}", e);
                }
            }
        });

        let response_stream = ReceiverStream::new(rx).map(Ok);
        Ok(tonic::Response::new(
            Box::pin(response_stream) as Self::GCTasksStreamStream
        ))
    }

    async fn heartbeat(
        &self,
        request: tonic::Request<Streaming<HeartbeatRequest>>,
    ) -> Result<tonic::Response<Self::HeartbeatStream>, tonic::Status> {
        let mut in_stream = request.into_inner();
        let (tx, rx) = mpsc::channel(4);
        let rx = DropReceiver { inner: rx };
        let coordinator = self.coordinator.clone();
        let mut shutdown_rx = self.shutdown_rx.clone();
        tokio::spawn(async move {
            let mut executor_id: Option<String> = None;
            loop {
                select! {
                    _ = shutdown_rx.changed() => {
                        info!("shutting down server, stopping heartbeats from executor: {:?}", executor_id);
                        break;
                    }
                    frame = in_stream.next() => {
                        // Ensure the frame has something
                        if frame.as_ref().is_none() {
                            break;
                        }
                        if let Err(err) = frame.as_ref().unwrap() {
                            info!("error receiving heartbeat request: {:?}", err);
                            break;
                        }
                        // We could have used Option<> here but it would be inconvenient to dereference
                        // it every time we need to use it below
                        if executor_id.is_none() {
                            if let Some(Ok(hb_request)) = frame {
                                executor_id.replace(hb_request.executor_id.clone());
                            }
                        }
                        if let Some(executor_id) = executor_id.clone() {
                            let tasks = coordinator.heartbeat(&executor_id).await;
                            match tasks {
                                Err(err) => {
                                    if let Err(err) = tx.send(Err(tonic::Status::internal(err.to_string()))).await {
                                        error!("error sending error message in heartbeat response: {}",err);
                                        break;
                                    }
                                }
                                Ok(tasks) => {
                                    let tasks = tasks.into_iter().map(|t| t.into()).collect::<Vec<indexify_coordinator::Task>>();
                                    let resp = HeartbeatResponse {
                                        executor_id: executor_id.clone(),
                                        tasks,
                                    };
                                    if let Err(err) = tx.send(Ok(resp)).await {
                                        error!("error sending heartbeat response: {:?}", err);
                                        break;
                                    }
                                }
                            }
                        }

                    }
                }
            }
            info!("heartbeats stopped, removing executor: {:?}", executor_id);
            if let Some(executor_id) = executor_id {
                if let Err(err) = coordinator.remove_executor(&executor_id).await {
                    error!("error removing executor: {}", err);
                }
            }
        });
        Ok(tonic::Response::new(Box::pin(rx) as HBResponseStream))
    }

    async fn update_task(
        &self,
        request: tonic::Request<UpdateTaskRequest>,
    ) -> Result<tonic::Response<UpdateTaskResponse>, tonic::Status> {
        let request = request.into_inner();
        let outcome: internal_api::TaskOutcome = request.outcome().into();
        let _ = self
            .coordinator
            .update_task(
                &request.task_id,
                &request.executor_id,
                outcome,
                request.content_list,
            )
            .await
            .map_err(|e| tonic::Status::aborted(e.to_string()))?;
        Ok(tonic::Response::new(UpdateTaskResponse {}))
    }

    async fn list_indexes(
        &self,
        request: Request<ListIndexesRequest>,
    ) -> Result<Response<ListIndexesResponse>, Status> {
        let request = request.into_inner();
        let indexes = self
            .coordinator
            .list_indexes(&request.namespace)
            .await
            .map_err(|e| tonic::Status::aborted(e.to_string()))?;
        let indexes = indexes
            .into_iter()
            .map(|i| i.into())
            .collect::<Vec<indexify_coordinator::Index>>();
        Ok(tonic::Response::new(ListIndexesResponse { indexes }))
    }

    async fn get_index(
        &self,
        request: Request<GetIndexRequest>,
    ) -> Result<Response<GetIndexResponse>, Status> {
        let request = request.into_inner();
        let index = self
            .coordinator
            .get_index(&request.namespace, &request.name)
            .await
            .map_err(|e| tonic::Status::aborted(e.to_string()))?;
        Ok(tonic::Response::new(GetIndexResponse {
            index: Some(index.into()),
        }))
    }

    async fn create_index(
        &self,
        request: Request<CreateIndexRequest>,
    ) -> Result<Response<CreateIndexResponse>, Status> {
        let request = request.into_inner();
        let index: internal_api::Index = request.index.unwrap().into();
        let namespace = index.namespace.clone();
        self.coordinator
            .create_index(&namespace, index)
            .await
            .map_err(|e| tonic::Status::aborted(e.to_string()))?;
        Ok(tonic::Response::new(CreateIndexResponse {}))
    }

    async fn get_extractor_coordinates(
        &self,
        req: Request<GetExtractorCoordinatesRequest>,
    ) -> Result<Response<indexify_coordinator::GetExtractorCoordinatesResponse>, Status> {
        let req = req.into_inner();
        let extractor_coordinates = self
            .coordinator
            .get_extractor_coordinates(&req.extractor)
            .await
            .map_err(|e| tonic::Status::aborted(e.to_string()))?;
        Ok(Response::new(
            indexify_coordinator::GetExtractorCoordinatesResponse {
                addrs: extractor_coordinates,
            },
        ))
    }

    async fn get_content_metadata(
        &self,
        req: Request<GetContentMetadataRequest>,
    ) -> Result<Response<indexify_coordinator::GetContentMetadataResponse>, Status> {
        let req = req.into_inner();
        let content_metadata_list = self
            .coordinator
            .get_content_metadata(req.content_list)
            .await
            .map_err(|e| tonic::Status::aborted(e.to_string()))?;
        let content_metadata = content_metadata_list
            .iter()
            .map(|c| (c.id.id.clone(), c.clone().into()))
            .collect::<HashMap<String, indexify_coordinator::ContentMetadata>>();
        Ok(Response::new(
            indexify_coordinator::GetContentMetadataResponse {
                content_list: content_metadata,
            },
        ))
    }

    async fn get_task(
        &self,
        req: Request<GetTaskRequest>,
    ) -> Result<Response<GetTaskResponse>, Status> {
        let req = req.into_inner();
        let task = self
            .coordinator
            .get_task(&req.task_id)
            .await
            .map_err(|e| tonic::Status::aborted(e.to_string()))?;
        Ok(Response::new(GetTaskResponse {
            task: Some(task.into()),
        }))
    }

    async fn get_ingestion_info(
        &self,
        req: Request<GetIngestionInfoRequest>,
    ) -> Result<Response<GetIngestionInfoResponse>, Status> {
        let req = req.into_inner();
        let (task, root_content) = self
            .coordinator
            .get_task_and_root_content(&req.task_id)
            .await
            .map_err(|e| tonic::Status::aborted(e.to_string()))?;

        let root_content: Option<indexify_coordinator::ContentMetadata> =
            root_content.map(|c| c.into());

        Ok(Response::new(GetIngestionInfoResponse {
            task: Some(task.into()),
            root_content,
        }))
    }

    async fn get_content_tree_metadata(
        &self,
        req: Request<GetContentTreeMetadataRequest>,
    ) -> Result<Response<indexify_coordinator::GetContentTreeMetadataResponse>, Status> {
        let req = req.into_inner();
        let content_tree_metadata = self
            .coordinator
            .get_content_tree_metadata(&req.content_id)
            .await
            .map_err(|e| tonic::Status::aborted(e.to_string()))?;
        let parsed_content_tree: Vec<indexify_coordinator::ContentMetadata> = content_tree_metadata
            .iter()
            .map(|c| c.clone().into())
            .collect();
        Ok(Response::new(
            indexify_coordinator::GetContentTreeMetadataResponse {
                content_list: parsed_content_tree,
            },
        ))
    }

    async fn list_state_changes(
        &self,
        _req: Request<ListStateChangesRequest>,
    ) -> Result<Response<indexify_coordinator::ListStateChangesResponse>, Status> {
        let state_changes = self
            .coordinator
            .list_state_changes()
            .await
            .map_err(|e| tonic::Status::aborted(e.to_string()))?
            .into_iter()
            .map(|c| c.into())
            .collect();
        Ok(Response::new(
            indexify_coordinator::ListStateChangesResponse {
                changes: state_changes,
            },
        ))
    }

    async fn list_tasks(
        &self,
        req: Request<ListTasksRequest>,
    ) -> Result<Response<ListTasksResponse>, Status> {
        let req = req.into_inner();
        let extraction_policy = if req.extraction_policy.is_empty() {
            None
        } else {
            Some(req.extraction_policy)
        };
        let tasks = self
            .coordinator
            .list_tasks(&req.namespace, extraction_policy)
            .await
            .map_err(|e| tonic::Status::aborted(e.to_string()))?;
        let tasks = tasks.into_iter().map(|t| t.into()).collect();
        Ok(Response::new(indexify_coordinator::ListTasksResponse {
            tasks,
        }))
    }

    async fn get_schema(
        &self,
        req: Request<GetSchemaRequest>,
    ) -> Result<Response<GetSchemaResponse>, Status> {
        let req = req.into_inner();
        let schema = self
            .coordinator
            .get_schema(&req.namespace, &req.content_source)
            .await
            .map_err(|e| tonic::Status::aborted(e.to_string()))?;
        Ok(Response::new(GetSchemaResponse {
            schema: Some(indexify_coordinator::StructuredDataSchema {
                columns: serde_json::to_string(&schema.columns).unwrap(),
                content_source: schema.content_source,
            }),
        }))
    }

    async fn list_schemas(
        &self,
        req: Request<GetAllSchemaRequest>,
    ) -> Result<Response<GetAllSchemaResponse>, Status> {
        let req = req.into_inner();
        let schemas = self
            .coordinator
            .list_schemas(&req.namespace)
            .await
            .map_err(|e| tonic::Status::aborted(e.to_string()))?;

        Ok(Response::new(GetAllSchemaResponse {
            schemas: schemas
                .into_iter()
                .map(|s| indexify_coordinator::StructuredDataSchema {
                    columns: serde_json::to_string(&s.columns).unwrap(),
                    content_source: s.content_source,
                })
                .collect(),
        }))
    }

    async fn get_raft_metrics_snapshot(
        &self,
        _req: Request<GetRaftMetricsSnapshotRequest>,
    ) -> Result<Response<RaftMetricsSnapshotResponse>, Status> {
        let metrics = self.coordinator.get_raft_metrics();
        let metrics_snapshot = metrics.raft_metrics;
        let openraft_metrics = metrics.openraft_metrics;

        // Conversion from MetricsSnapshot to RaftMetricsSnapshotResponse
        let response = RaftMetricsSnapshotResponse {
            fail_connect_to_peer: metrics_snapshot.fail_connect_to_peer,
            sent_bytes: metrics_snapshot.sent_bytes,
            recv_bytes: metrics_snapshot.recv_bytes,
            sent_failures: metrics_snapshot.sent_failures,
            snapshot_send_success: metrics_snapshot.snapshot_send_success,
            snapshot_send_failure: metrics_snapshot.snapshot_send_failure,
            snapshot_recv_success: metrics_snapshot.snapshot_recv_success,
            snapshot_recv_failure: metrics_snapshot.snapshot_recv_failure,
            snapshot_send_inflights: metrics_snapshot.snapshot_send_inflights,
            snapshot_recv_inflights: metrics_snapshot.snapshot_recv_inflights,
            snapshot_sent_seconds: metrics_snapshot
                .snapshot_sent_seconds
                .into_iter()
                .map(|(k, v)| {
                    (
                        k,
                        Uint64List {
                            values: v.into_iter().map(|d| d.as_millis() as u64).collect(),
                        },
                    )
                })
                .collect(),
            snapshot_recv_seconds: metrics_snapshot
                .snapshot_recv_seconds
                .into_iter()
                .map(|(k, v)| {
                    (
                        k,
                        Uint64List {
                            values: v.into_iter().map(|d| d.as_millis() as u64).collect(),
                        },
                    )
                })
                .collect(),
            snapshot_size: metrics_snapshot.snapshot_size,
            last_snapshot_creation_time_millis: metrics_snapshot
                .last_snapshot_creation_time
                .as_millis() as u64,
            running_state_ok: openraft_metrics.running_state.is_ok(),
            id: openraft_metrics.id,
            current_term: openraft_metrics.current_term,
            vote: openraft_metrics.vote.leader_id.node_id,
            last_log_index: openraft_metrics.last_log_index.unwrap_or(0),
            current_leader: openraft_metrics.current_leader.unwrap_or(0),
        };

        Ok(Response::new(response))
    }

    async fn get_all_task_assignments(
        &self,
        _req: Request<GetAllTaskAssignmentRequest>,
    ) -> Result<Response<TaskAssignments>, Status> {
        let assignments = self
            .coordinator
            .all_task_assignments()
            .await
            .map_err(|e| tonic::Status::aborted(e.to_string()))?;
        Ok(Response::new(TaskAssignments { assignments }))
    }
}

pub struct CoordinatorServer {
    addr: SocketAddr,
    coordinator: Arc<Coordinator>,
    shared_state: Arc<state::App>,
    config: Arc<ServerConfig>,
    server_handle: axum_server::Handle,
}

async fn metrics_handler(
    State(app): State<Arc<state::App>>,
) -> Result<axum::response::Response<axum::body::Body>, IndexifyAPIError> {
    let metric_families = app.registry.gather();
    let mut buffer = vec![];
    let encoder = prometheus::TextEncoder::new();
    encoder.encode(&metric_families, &mut buffer).map_err(|_| {
        IndexifyAPIError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to encode metrics",
        )
    })?;

    Ok(axum::response::Response::new(axum::body::Body::from(
        buffer,
    )))
}

use std::borrow::Cow;

#[derive(Debug, Clone, Default)]
struct TraceLayer {
    name: &'static str,
}

impl<S> Layer<S> for TraceLayer {
    type Service = TraceWrapper<S>;

    fn layer(&self, service: S) -> Self::Service {
        TraceWrapper::new(self.name, service)
    }
}

#[derive(Clone)]
struct Metrics {
    pub req_active: UpDownCounter<i64>,

    pub req_duration: Histogram<f64>,
}

impl Metrics {
    pub fn new(name: impl Into<Cow<'static, str>>) -> Self {
        let meter = opentelemetry::global::meter(name);

        let req_active = meter
            .i64_up_down_counter("grpc.server.req_active")
            .with_description("Number of active requests")
            .init();
        let req_duration = meter
            .f64_histogram("grpc.server.req_duration")
            .with_description("Request duration in seconds")
            .init();
        Self {
            req_active,
            req_duration,
        }
    }
}

#[derive(Clone)]
struct TraceWrapper<S> {
    inner: S,

    metrics: Metrics,
}

impl<S> TraceWrapper<S> {
    pub fn new(name: impl Into<Cow<'static, str>>, inner: S) -> Self {
        Self {
            inner,
            metrics: Metrics::new(name),
        }
    }
}

type BoxFuture<'a, T> = Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

use tracing_opentelemetry::OpenTelemetrySpanExt;

impl<S, ReqBody> Service<tonic::codegen::http::request::Request<ReqBody>> for TraceWrapper<S>
where
    S: Service<
            tonic::codegen::http::request::Request<ReqBody>,
            Response = tonic::codegen::http::response::Response<BoxBody>,
        > + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
    ReqBody: Send + 'static,
{
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;
    type Response = S::Response;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: tonic::codegen::http::request::Request<ReqBody>) -> Self::Future {
        let mut inner = self.inner.clone();
        let metrics = self.metrics.clone();

        let remote_ctx =
            global::get_text_map_propagator(|prop| prop.extract(&MetadataMap(req.headers())));

        let uri_path = req.uri().path().to_string();
        let span = tracing::span!(tracing::Level::TRACE, "grpc", uri=%uri_path);
        span.set_parent(remote_ctx);

        let future = async move {
            let start = std::time::Instant::now();

            let mut labels = [
                KeyValue::new("grpc.method", uri_path),
                KeyValue::new("grpc.status", 0i64),
            ];

            metrics.req_active.add(1, &[]);

            let out = inner.call(req).await;

            metrics.req_active.add(-1, &[]);

            labels[1].value = if let Ok(response) = &out {
                (response.status().as_u16() as i64).into()
            } else {
                1i64.into()
            };

            metrics
                .req_duration
                .record(start.elapsed().as_secs_f64(), &labels);

            out
        }
        .instrument(span);

        Box::pin(future)
    }
}

fn start_server(app: &CoordinatorServer) -> Result<JoinHandle<Result<()>>> {
    let server = axum::Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(app.shared_state.clone());
    let addr: SocketAddr = format!(
        "{}:{}",
        app.config.listen_if, app.config.coordinator_http_port
    )
    .parse()?;
    let handle = app.server_handle.clone();

    Ok(tokio::spawn(async move {
        axum_server::bind(addr)
            .handle(handle)
            .serve(server.into_make_service())
            .await?;
        Ok(())
    }))
}

impl CoordinatorServer {
    pub async fn new(
        config: Arc<ServerConfig>,
        registry: Arc<prometheus::Registry>,
    ) -> Result<Self, anyhow::Error> {
        let addr: SocketAddr = config.coordinator_lis_addr_sock()?;
        let garbage_collector = GarbageCollector::new();
        let shared_state = state::App::new(
            config.clone(),
            None,
            Arc::clone(&garbage_collector),
            &config.coordinator_addr,
            registry,
        )
        .await?;
        let coordinator_client = CoordinatorClient::new(Arc::clone(&config));

        let coordinator = Coordinator::new(
            shared_state.clone(),
            coordinator_client,
            Arc::clone(&garbage_collector),
        );
        info!("coordinator listening on: {}", addr.to_string());
        Ok(Self {
            addr,
            coordinator,
            shared_state,
            config,
            server_handle: axum_server::Handle::new(),
        })
    }

    pub async fn run(&self) -> Result<(), anyhow::Error> {
        let (shutdown_tx, shutdown_rx) = watch::channel(());
        let svc = CoordinatorServiceServer {
            coordinator: self.coordinator.clone(),
            shutdown_rx: shutdown_rx.clone(),
        };
        let srvr =
            indexify_coordinator::coordinator_service_server::CoordinatorServiceServer::new(svc);
        let shared_state = self.shared_state.clone();
        shared_state
            .initialize_raft()
            .await
            .map_err(|e| anyhow!("unable to initialize shared state: {}", e.to_string()))?;
        let leader_change_watcher = self.coordinator.get_leader_change_watcher();
        let coordinator_clone = self.coordinator.clone();
        let state_watcher_rx = self.coordinator.get_state_watcher();
        if let Err(e) = start_server(self) {
            error!("unable to start metrics server: {}", e);
        }
        tokio::spawn(async move {
            let _ = run_scheduler(
                shutdown_rx,
                leader_change_watcher,
                state_watcher_rx,
                coordinator_clone,
            )
            .await;
        });

        let layer = ServiceBuilder::new()
            .layer(TraceLayer {
                name: "indexify-coordinator-grpc",
            })
            .into_inner();

        if let Some(tls_config) = self.config.coordinator_tls.as_ref() {
            if tls_config.api {
                tracing::info!("starting coordinator grpc server with TLS enabled");
                let cert = tokio::fs::read(tls_config.cert_file.clone()).await?;
                let key = tokio::fs::read(tls_config.key_file.clone()).await?;
                let identity = tonic::transport::Identity::from_pem(cert, key);

                let mut tonic_tls_config =
                    tonic::transport::ServerTlsConfig::new().identity(identity);
                if let Some(ca_file) = &tls_config.ca_file {
                    let client_ca_cert = tokio::fs::read(ca_file).await?;
                    let client_ca_cert = tonic::transport::Certificate::from_pem(client_ca_cert);
                    tonic_tls_config = tonic_tls_config.client_ca_root(client_ca_cert);
                }

                tonic::transport::Server::builder()
                    .tls_config(tonic_tls_config)?
                    .layer(layer)
                    .add_service(srvr)
                    .serve_with_shutdown(self.addr, async move {
                        let _ = shutdown_signal(shutdown_tx).await;
                        let res = shared_state.stop().await;
                        if let Err(err) = res {
                            error!("error stopping server: {:?}", err);
                        }
                        self.server_handle.shutdown();
                    })
                    .await
                    .map_err(|e| {
                        anyhow!(
                            "unable to start grpc server: {} addr: {}",
                            e.to_string(),
                            self.addr
                        )
                    })?;
                return Ok(());
            }
        }

        tracing::info!("starting coordinator grpc server with TLS disabled");
        tonic::transport::Server::builder()
            .layer(layer)
            .add_service(srvr)
            .serve_with_shutdown(self.addr, async move {
                let _ = shutdown_signal(shutdown_tx).await;
                let res = shared_state.stop().await;
                if let Err(err) = res {
                    error!("error stopping server: {:?}", err);
                }
                self.server_handle.shutdown();
            })
            .await
            .map_err(|e| {
                anyhow!(
                    "unable to start grpc server: {} addr: {}",
                    e.to_string(),
                    self.addr
                )
            })?;
        Ok(())
    }

    // Used only for tests
    pub fn get_coordinator(&self) -> Arc<Coordinator> {
        self.coordinator.clone()
    }
}

async fn run_scheduler(
    mut shutdown_rx: Receiver<()>,
    mut leader_changed: Receiver<bool>,
    mut state_watcher_rx: Receiver<StateChange>,
    coordinator: Arc<Coordinator>,
) -> Result<()> {
    let is_leader = AtomicBool::new(false);

    loop {
        tokio::select! {
            _ = state_watcher_rx.changed() => {
                if is_leader.load(Ordering::Relaxed) {
                   let _state_change = state_watcher_rx.borrow_and_update().clone();
                   if let Err(err) = coordinator.run_scheduler().await {
                          error!("error processing and distributing work: {:?}", err);
                   }
                }
            },
            _ = shutdown_rx.changed() => {
                info!("scheduler shutting down");
                break;
            }
            _ = leader_changed.changed() => {
                let leader_state = *leader_changed.borrow_and_update();
                info!("leader changed detected: {:?}", leader_state);
                is_leader.store(leader_state, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
    Ok(())
}

#[tracing::instrument]
async fn shutdown_signal(shutdown_tx: Sender<()>) {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
        },
        _ = terminate => {
        },
    }
    shutdown_tx.send(()).unwrap();
    info!("signal received, shutting down server gracefully");
}
