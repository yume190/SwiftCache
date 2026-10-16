//! Storage-backed implementations of the two remote caching services.

use std::sync::Arc;

use prost::Message;
use tonic::{Request, Response, Status};
use tracing::{info, warn};

use crate::blob::{self, BlobDir};
use crate::hash::short_id;
use crate::proto::cas::casdb_service_server::CasdbService;
use crate::proto::cas::{
    cas_get_response, cas_load_response, cas_put_response, cas_save_response, CasBlob, CasDataId,
    CasGetRequest, CasGetResponse, CasLoadRequest, CasLoadResponse, CasObject, CasPutRequest,
    CasPutResponse, CasSaveRequest, CasSaveResponse,
};
use crate::proto::kv::key_value_db_server::KeyValueDb;
use crate::proto::kv::{
    get_value_response, GetValueRequest, GetValueResponse, PutValueRequest, PutValueResponse, Value,
};
use crate::peer::Peer;
use crate::store::{human_bytes, valid_id, Store};

pub struct CasService {
    pub store: Arc<Store>,
    pub blob_dir: Option<Arc<BlobDir>>,
}

pub struct KvService {
    pub store: Arc<Store>,
}

fn internal(e: anyhow::Error) -> Status {
    warn!("request failed: {e:#}");
    Status::internal(format!("{e:#}"))
}

#[tonic::async_trait]
impl CasdbService for CasService {
    /// Stores an object with references. Returns the content-derived id.
    async fn put(&self, req: Request<CasPutRequest>) -> Result<Response<CasPutResponse>, Status> {
        let peer = Peer::of(&req);
        self.store.stats.note_peer(peer);
        let data = req.into_inner().data.unwrap_or_default();
        let refs: Vec<Vec<u8>> = data.references.into_iter().map(|r| r.id).collect();
        let payload = blob::read(&data.blob).await.map_err(internal)?;
        let len = payload.len();
        let nrefs = refs.len();
        let id = self
            .store
            .put_object(refs, payload)
            .await
            .map_err(internal)?;
        info!(
            "{peer} up   cas put  {} refs={nrefs} {}",
            short_id(&id),
            human_bytes(len as u64)
        );
        Ok(Response::new(CasPutResponse {
            contents: Some(cas_put_response::Contents::CasId(CasDataId { id })),
        }))
    }

    /// Loads an object with its references.
    async fn get(&self, req: Request<CasGetRequest>) -> Result<Response<CasGetResponse>, Status> {
        let peer = Peer::of(&req);
        self.store.stats.note_peer(peer);
        let req = req.into_inner();
        let id = req.cas_id.unwrap_or_default().id;
        if !valid_id(&id) {
            return Ok(Response::new(not_found_get()));
        }
        let Some(obj) = self.store.get_object(&id).await.map_err(internal)? else {
            info!("{peer} ---  cas get  {} miss", short_id(&id));
            return Ok(Response::new(not_found_get()));
        };
        info!(
            "{peer} down cas get  {} refs={} {}",
            short_id(&id),
            obj.refs.len(),
            human_bytes(obj.data.len() as u64)
        );
        let blob = blob::respond(obj.data, req.write_to_disk, self.blob_dir.as_deref())
            .await
            .map_err(internal)?;
        Ok(Response::new(CasGetResponse {
            outcome: cas_get_response::Outcome::Success as i32,
            contents: Some(cas_get_response::Contents::Data(CasObject {
                blob: Some(blob),
                references: obj
                    .refs
                    .into_iter()
                    .map(|id| CasDataId { id })
                    .collect(),
            })),
        }))
    }

    /// Stores a bare blob (no references).
    async fn save(&self, req: Request<CasSaveRequest>) -> Result<Response<CasSaveResponse>, Status> {
        let peer = Peer::of(&req);
        self.store.stats.note_peer(peer);
        let data = req.into_inner().data.unwrap_or_default();
        let payload = blob::read(&data.blob).await.map_err(internal)?;
        let len = payload.len();
        let id = self
            .store
            .put_object(Vec::new(), payload)
            .await
            .map_err(internal)?;
        info!(
            "{peer} up   cas save {} {}",
            short_id(&id),
            human_bytes(len as u64)
        );
        Ok(Response::new(CasSaveResponse {
            contents: Some(cas_save_response::Contents::CasId(CasDataId { id })),
        }))
    }

    /// Loads a bare blob, ignoring any references the object may carry.
    async fn load(&self, req: Request<CasLoadRequest>) -> Result<Response<CasLoadResponse>, Status> {
        let peer = Peer::of(&req);
        self.store.stats.note_peer(peer);
        let req = req.into_inner();
        let id = req.cas_id.unwrap_or_default().id;
        if !valid_id(&id) {
            return Ok(Response::new(not_found_load()));
        }
        let Some(obj) = self.store.get_object(&id).await.map_err(internal)? else {
            info!("{peer} ---  cas load {} miss", short_id(&id));
            return Ok(Response::new(not_found_load()));
        };
        info!(
            "{peer} down cas load {} {}",
            short_id(&id),
            human_bytes(obj.data.len() as u64)
        );
        let blob = blob::respond(obj.data, req.write_to_disk, self.blob_dir.as_deref())
            .await
            .map_err(internal)?;
        Ok(Response::new(CasLoadResponse {
            outcome: cas_load_response::Outcome::Success as i32,
            contents: Some(cas_load_response::Contents::Data(CasBlob {
                blob: Some(blob),
            })),
        }))
    }
}

fn not_found_get() -> CasGetResponse {
    CasGetResponse {
        outcome: cas_get_response::Outcome::ObjectNotFound as i32,
        contents: None,
    }
}

fn not_found_load() -> CasLoadResponse {
    CasLoadResponse {
        outcome: cas_load_response::Outcome::ObjectNotFound as i32,
        contents: None,
    }
}

#[tonic::async_trait]
impl KeyValueDb for KvService {
    /// Action cache lookup. A miss is `KEY_NOT_FOUND`, never a gRPC error:
    /// the compiler treats transport errors as fatal when
    /// `COMPILATION_CACHE_ENABLE_STRICT_CAS_ERRORS` is on.
    async fn get_value(
        &self,
        req: Request<GetValueRequest>,
    ) -> Result<Response<GetValueResponse>, Status> {
        let peer = Peer::of(&req);
        self.store.stats.note_peer(peer);
        let key = req.into_inner().key;
        let Some(raw) = self.store.get_value(&key).await.map_err(internal)? else {
            info!("{peer} ---  kv  get  {} miss", short_id(&key));
            return Ok(Response::new(GetValueResponse {
                outcome: get_value_response::Outcome::KeyNotFound as i32,
                contents: None,
            }));
        };
        let value = Value::decode(raw.as_slice())
            .map_err(|e| internal(anyhow::Error::new(e).context("decoding stored value")))?;
        info!(
            "{peer} down kv  get  {} hit entries={}",
            short_id(&key),
            value.entries.len()
        );
        Ok(Response::new(GetValueResponse {
            outcome: get_value_response::Outcome::Success as i32,
            contents: Some(get_value_response::Contents::Value(value)),
        }))
    }

    async fn put_value(
        &self,
        req: Request<PutValueRequest>,
    ) -> Result<Response<PutValueResponse>, Status> {
        let peer = Peer::of(&req);
        self.store.stats.note_peer(peer);
        let req = req.into_inner();
        let value = req.value.unwrap_or_default();
        info!(
            "{peer} up   kv  put  {} entries={}",
            short_id(&req.key),
            value.entries.len()
        );
        self.store
            .put_value(&req.key, &value.encode_to_vec())
            .await
            .map_err(internal)?;
        Ok(Response::new(PutValueResponse { error: None }))
    }
}
