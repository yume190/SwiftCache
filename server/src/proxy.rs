//! Unix-socket front end that forwards to a real remote server over TCP.
//!
//! The toolchain's CAS plugin can only be pointed at a unix socket
//! (`remote-service-path`), so talking to a cache shared across machines
//! requires a local hop. This proxy is that hop: it speaks the same protocol on
//! both sides and only rewrites the parts that are filesystem-local
//! (`CASBytes.file_path`), which cannot cross a machine boundary.

use std::sync::Arc;

use tonic::transport::Channel;
use tonic::{Request, Response, Status};
use tracing::info;

use crate::blob::{self, BlobDir};
use crate::proto::cas::casdb_service_client::CasdbServiceClient;
use crate::proto::cas::casdb_service_server::CasdbService;
use crate::proto::cas::{
    cas_bytes, cas_get_response, cas_load_response, CasBlob, CasBytes, CasGetRequest,
    CasGetResponse, CasLoadRequest, CasLoadResponse, CasObject, CasPutRequest, CasPutResponse,
    CasSaveRequest, CasSaveResponse,
};
use crate::proto::kv::key_value_db_client::KeyValueDbClient;
use crate::proto::kv::key_value_db_server::KeyValueDb;
use crate::proto::kv::{GetValueRequest, GetValueResponse, PutValueRequest, PutValueResponse};
use crate::peer::Peer;
use crate::store::human_bytes;

pub struct CasProxy {
    pub upstream: CasdbServiceClient<Channel>,
    pub blob_dir: Option<Arc<BlobDir>>,
}

pub struct KvProxy {
    pub upstream: KeyValueDbClient<Channel>,
}

fn io(e: anyhow::Error) -> Status {
    Status::internal(format!("{e:#}"))
}

/// Replaces a `file_path` blob with its contents so it can cross the network.
async fn inline(bytes: Option<CasBytes>) -> Result<Option<CasBytes>, Status> {
    match bytes.as_ref().and_then(|b| b.contents.as_ref()) {
        Some(cas_bytes::Contents::FilePath(_)) => {
            let data = blob::read(&bytes).await.map_err(io)?;
            Ok(Some(CasBytes {
                contents: Some(cas_bytes::Contents::Data(data)),
            }))
        }
        _ => Ok(bytes),
    }
}

#[tonic::async_trait]
impl CasdbService for CasProxy {
    async fn put(&self, req: Request<CasPutRequest>) -> Result<Response<CasPutResponse>, Status> {
        let peer = Peer::of(&req);
        let mut req = req.into_inner();
        if let Some(obj) = req.data.as_mut() {
            obj.blob = inline(obj.blob.take()).await?;
        }
        let len = req
            .data
            .as_ref()
            .map(|o| blob::len(&o.blob))
            .unwrap_or_default();
        info!("{peer} up   cas put  → upstream {}", human_bytes(len as u64));
        self.upstream.clone().put(req).await
    }

    async fn save(&self, req: Request<CasSaveRequest>) -> Result<Response<CasSaveResponse>, Status> {
        let peer = Peer::of(&req);
        let mut req = req.into_inner();
        if let Some(blob) = req.data.as_mut() {
            blob.blob = inline(blob.blob.take()).await?;
        }
        let len = req
            .data
            .as_ref()
            .map(|b| blob::len(&b.blob))
            .unwrap_or_default();
        info!("{peer} up   cas save → upstream {}", human_bytes(len as u64));
        self.upstream.clone().save(req).await
    }

    async fn get(&self, req: Request<CasGetRequest>) -> Result<Response<CasGetResponse>, Status> {
        let peer = Peer::of(&req);
        let mut req = req.into_inner();
        let want_file = std::mem::replace(&mut req.write_to_disk, false);
        let mut resp = self.upstream.clone().get(req).await?.into_inner();
        if resp.outcome == cas_get_response::Outcome::Success as i32 {
            if let Some(cas_get_response::Contents::Data(obj)) = resp.contents.take() {
                let data = blob::read(&obj.blob).await.map_err(io)?;
                info!(
                    "{peer} down cas get  ← upstream {}",
                    human_bytes(data.len() as u64)
                );
                let blob = blob::respond(data, want_file, self.blob_dir.as_deref())
                    .await
                    .map_err(io)?;
                resp.contents = Some(cas_get_response::Contents::Data(CasObject {
                    blob: Some(blob),
                    references: obj.references,
                }));
            }
        } else {
            info!("{peer} ---  cas get  ← upstream miss");
        }
        Ok(Response::new(resp))
    }

    async fn load(&self, req: Request<CasLoadRequest>) -> Result<Response<CasLoadResponse>, Status> {
        let peer = Peer::of(&req);
        let mut req = req.into_inner();
        let want_file = std::mem::replace(&mut req.write_to_disk, false);
        let mut resp = self.upstream.clone().load(req).await?.into_inner();
        if resp.outcome == cas_load_response::Outcome::Success as i32 {
            if let Some(cas_load_response::Contents::Data(b)) = resp.contents.take() {
                let data = blob::read(&b.blob).await.map_err(io)?;
                info!(
                    "{peer} down cas load ← upstream {}",
                    human_bytes(data.len() as u64)
                );
                let blob = blob::respond(data, want_file, self.blob_dir.as_deref())
                    .await
                    .map_err(io)?;
                resp.contents = Some(cas_load_response::Contents::Data(CasBlob {
                    blob: Some(blob),
                }));
            }
        } else {
            info!("{peer} ---  cas load ← upstream miss");
        }
        Ok(Response::new(resp))
    }
}

#[tonic::async_trait]
impl KeyValueDb for KvProxy {
    async fn get_value(
        &self,
        req: Request<GetValueRequest>,
    ) -> Result<Response<GetValueResponse>, Status> {
        let peer = Peer::of(&req);
        info!("{peer} ---  kv  get  → upstream");
        self.upstream.clone().get_value(req.into_inner()).await
    }

    async fn put_value(
        &self,
        req: Request<PutValueRequest>,
    ) -> Result<Response<PutValueResponse>, Status> {
        let peer = Peer::of(&req);
        info!("{peer} up   kv  put  → upstream");
        self.upstream.clone().put_value(req.into_inner()).await
    }
}
