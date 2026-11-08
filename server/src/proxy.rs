//! Unix-socket front end that forwards to a real remote server over TCP.
//!
//! The toolchain's CAS plugin can only be pointed at a unix socket
//! (`remote-service-path`), so talking to a cache shared across machines
//! requires a local hop. This proxy is that hop: it speaks the same protocol on
//! both sides and only rewrites the parts that are filesystem-local
//! (`CASBytes.file_path`), which cannot cross a machine boundary.
//!
//! Every forwarded RPC is recorded in the [journal](crate::journal), including the
//! `file_path` contents before they are inlined.

use std::sync::Arc;

use prost::Message;
use serde_json::json;
use tonic::transport::Channel;
use tonic::{Request, Response, Status};
use tracing::info;

use crate::blob::{self, BlobDir};
use crate::journal::{blob_json, entries_json, refs_json, Rec, Recorder};
use crate::peer::Peer;
use crate::proto::cas::casdb_service_client::CasdbServiceClient;
use crate::proto::cas::casdb_service_server::CasdbService;
use crate::proto::cas::{
    cas_bytes, cas_get_response, cas_load_response, cas_put_response, cas_save_response, CasBlob, CasBytes, CasDataId,
    CasGetRequest, CasGetResponse, CasLoadRequest, CasLoadResponse, CasObject, CasPutRequest, CasPutResponse,
    CasSaveRequest, CasSaveResponse,
};
use crate::proto::kv::key_value_db_client::KeyValueDbClient;
use crate::proto::kv::key_value_db_server::KeyValueDb;
use crate::proto::kv::{
    get_value_response, GetValueRequest, GetValueResponse, PutValueRequest, PutValueResponse, Value,
};
use crate::store::{human_bytes, valid_id, Staged, Store};

pub struct CasProxy {
    pub upstream: CasdbServiceClient<Channel>,
    pub blob_dir: Option<Arc<BlobDir>>,
    pub journal: Recorder,
    pub store: Arc<Store>,
}

pub struct KvProxy {
    pub upstream: KeyValueDbClient<Channel>,
    pub journal: Recorder,
    pub store: Arc<Store>,
}

fn io(e: anyhow::Error) -> Status {
    Status::internal(format!("{e:#}"))
}

/// Replaces a `file_path` blob with its contents so it can cross the network.
async fn inline(bytes: Option<CasBytes>) -> Result<Option<CasBytes>, Status> {
    match bytes.as_ref().and_then(|b| b.contents.as_ref()) {
        Some(cas_bytes::Contents::FilePath(_)) => {
            let data = blob::read(bytes).await.map_err(io)?;
            Ok(Some(CasBytes {
                contents: Some(cas_bytes::Contents::Data(data)),
            }))
        }
        _ => Ok(bytes),
    }
}

/// Describes the request blob (as the client sent it, so `file_path` is kept)
/// before inlining.
fn describe_request_blob(rec: &mut Rec, blob: Option<&CasBytes>) {
    rec.request["blob"] = blob_json(blob);
}

/// Stores the (already inlined) request blob bytes; returns its payload
/// digest and length for [`Rec::remember`] once the upstream id is known.
fn record_request_blob(rec: &mut Rec, blob: Option<&CasBytes>) -> Option<(String, usize)> {
    let Some(cas_bytes::Contents::Data(d)) = blob.and_then(|b| b.contents.as_ref()) else {
        return None;
    };
    let digest = rec.payload("request.blob", d);
    rec.request["blob"]["bytes"] = json!(d.len());
    rec.request["blob"]["payload"] = json!(digest);
    Some((digest, d.len()))
}

/// Marks a non-success, non-miss upstream outcome as an error.
fn upstream_error(rec: &mut Rec, description: Option<&str>) {
    rec.fail(description.unwrap_or("upstream returned an error outcome"));
}

/// Whether a response blob actually carries a payload (an empty `CASBytes`
/// is a malformed response, not an empty object).
fn has_payload(blob: &Option<CasBytes>) -> bool {
    matches!(blob, Some(CasBytes { contents: Some(_) }))
}

/// Writes an upload's (already inlined) bytes and references to a staging
/// file; the bytes are moved out of the request and back, not copied.
async fn stage_upload(store: &Store, refs: Vec<Vec<u8>>, blob: &mut Option<CasBytes>) -> Result<Staged, Status> {
    let data = match blob.as_mut().and_then(|b| b.contents.as_mut()) {
        Some(cas_bytes::Contents::Data(d)) => std::mem::take(d),
        _ => Vec::new(),
    };
    let (staged, data) = store.stage(refs, data).await.map_err(io)?;
    if let Some(cas_bytes::Contents::Data(d)) = blob.as_mut().and_then(|b| b.contents.as_mut()) {
        *d = data;
    }
    Ok(staged)
}

impl CasProxy {
    async fn do_put(
        &self,
        rec: &mut Rec,
        peer: Peer,
        mut req: CasPutRequest,
    ) -> Result<Response<CasPutResponse>, Status> {
        rec.request = json!({
            "references": refs_json(
                req.data.iter().flat_map(|o| o.references.iter().map(|r| r.id.as_slice()))
            ),
        });
        describe_request_blob(rec, req.data.as_ref().and_then(|o| o.blob.as_ref()));
        if let Some(obj) = req.data.as_mut() {
            obj.blob = inline(obj.blob.take()).await?;
        }
        let recorded = record_request_blob(rec, req.data.as_ref().and_then(|o| o.blob.as_ref()));
        let len = req.data.as_ref().map(|o| blob::len(&o.blob)).unwrap_or_default();
        let refs: Vec<Vec<u8>> = req
            .data
            .iter()
            .flat_map(|o| o.references.iter().map(|r| r.id.clone()))
            .collect();
        let staged = match req.data.as_mut() {
            Some(obj) => stage_upload(&self.store, refs, &mut obj.blob).await?,
            None => stage_upload(&self.store, refs, &mut None).await?,
        };
        info!("{peer} up   cas put  → upstream {}", human_bytes(len as u64));
        let resp = self.upstream.clone().put(req).await?;
        match &resp.get_ref().contents {
            Some(cas_put_response::Contents::CasId(id)) => {
                if let Some((digest, len)) = &recorded {
                    rec.remember(&id.id, digest, *len);
                }
                rec.object(&id.id);
                rec.response = json!({ "cas_id": hex::encode(&id.id) });
                if valid_id(&id.id) {
                    staged.commit(&id.id).await.map_err(io)?;
                }
            }
            Some(cas_put_response::Contents::Error(e)) => {
                rec.response = json!({ "error": e.description });
                upstream_error(rec, Some(&e.description));
            }
            None => {
                rec.response = json!({});
                upstream_error(rec, None);
            }
        }
        Ok(resp)
    }

    async fn do_save(
        &self,
        rec: &mut Rec,
        peer: Peer,
        mut req: CasSaveRequest,
    ) -> Result<Response<CasSaveResponse>, Status> {
        rec.request = json!({});
        describe_request_blob(rec, req.data.as_ref().and_then(|b| b.blob.as_ref()));
        if let Some(blob) = req.data.as_mut() {
            blob.blob = inline(blob.blob.take()).await?;
        }
        let recorded = record_request_blob(rec, req.data.as_ref().and_then(|b| b.blob.as_ref()));
        let len = req.data.as_ref().map(|b| blob::len(&b.blob)).unwrap_or_default();
        let staged = match req.data.as_mut() {
            Some(blob) => stage_upload(&self.store, Vec::new(), &mut blob.blob).await?,
            None => stage_upload(&self.store, Vec::new(), &mut None).await?,
        };
        info!("{peer} up   cas save → upstream {}", human_bytes(len as u64));
        let resp = self.upstream.clone().save(req).await?;
        match &resp.get_ref().contents {
            Some(cas_save_response::Contents::CasId(id)) => {
                if let Some((digest, len)) = &recorded {
                    rec.remember(&id.id, digest, *len);
                }
                rec.object(&id.id);
                rec.response = json!({ "cas_id": hex::encode(&id.id) });
                if valid_id(&id.id) {
                    staged.commit(&id.id).await.map_err(io)?;
                }
            }
            Some(cas_save_response::Contents::Error(e)) => {
                rec.response = json!({ "error": e.description });
                upstream_error(rec, Some(&e.description));
            }
            None => {
                rec.response = json!({});
                upstream_error(rec, None);
            }
        }
        Ok(resp)
    }

    async fn do_get(
        &self,
        rec: &mut Rec,
        peer: Peer,
        mut req: CasGetRequest,
    ) -> Result<Response<CasGetResponse>, Status> {
        // Owned: `req` moves upstream, and the id keys payload reuse on the way back.
        let id = req.cas_id.as_ref().map(|i| i.id.clone()).unwrap_or_default();
        rec.object(&id);
        rec.request = json!({
            "cas_id": hex::encode(&id),
            "write_to_disk": req.write_to_disk,
        });
        let cacheable = valid_id(&id);
        if cacheable {
            if let Some(obj) = self.store.get_object(&id).await.map_err(io)? {
                info!(
                    "{peer} down cas get  ← local cache {}",
                    human_bytes(obj.data.len() as u64)
                );
                let digest = rec.object_payload("response.blob", &id, &obj.data);
                let references = refs_json(obj.refs.iter().map(|r| r.as_slice()));
                let blob = blob::respond(obj.data, req.write_to_disk, self.blob_dir.as_deref())
                    .await
                    .map_err(io)?;
                rec.track(&blob);
                let mut described = blob_json(Some(&blob));
                described["payload"] = json!(digest);
                rec.outcome("hit");
                rec.response = json!({
                    "outcome": "success",
                    "references": references,
                    "blob": described,
                });
                return Ok(Response::new(CasGetResponse {
                    outcome: cas_get_response::Outcome::Success as i32,
                    contents: Some(cas_get_response::Contents::Data(CasObject {
                        blob: Some(blob),
                        references: obj.refs.into_iter().map(|id| CasDataId { id }).collect(),
                    })),
                }));
            }
        }
        let want_file = std::mem::replace(&mut req.write_to_disk, false);
        let mut resp = self.upstream.clone().get(req).await?.into_inner();
        if resp.outcome == cas_get_response::Outcome::Success as i32 {
            if let Some(cas_get_response::Contents::Data(obj)) = resp.contents.take() {
                let complete = has_payload(&obj.blob);
                let mut data = blob::read(obj.blob).await.map_err(io)?;
                info!("{peer} down cas get  ← upstream {}", human_bytes(data.len() as u64));
                if cacheable && complete {
                    let refs = obj.references.iter().map(|r| r.id.clone()).collect();
                    data = self.store.cache_object(&id, refs, data).await.map_err(io)?;
                }
                let digest = rec.object_payload("response.blob", &id, &data);
                let references = refs_json(obj.references.iter().map(|r| r.id.as_slice()));
                let blob = blob::respond(data, want_file, self.blob_dir.as_deref())
                    .await
                    .map_err(io)?;
                rec.track(&blob);
                let mut described = blob_json(Some(&blob));
                described["payload"] = json!(digest);
                rec.outcome("hit");
                rec.response = json!({
                    "outcome": "success",
                    "references": references,
                    "blob": described,
                });
                resp.contents = Some(cas_get_response::Contents::Data(CasObject {
                    blob: Some(blob),
                    references: obj.references,
                }));
            } else {
                rec.response = json!({ "outcome": "success" });
                upstream_error(rec, Some("upstream success without data"));
            }
        } else {
            info!("{peer} ---  cas get  ← upstream miss");
            if resp.outcome == cas_get_response::Outcome::ObjectNotFound as i32 {
                rec.outcome("miss");
                rec.response = json!({ "outcome": "object_not_found" });
            } else {
                let desc = match &resp.contents {
                    Some(cas_get_response::Contents::Error(e)) => Some(e.description.clone()),
                    _ => None,
                };
                rec.response = json!({ "outcome": "error", "error": desc });
                upstream_error(rec, desc.as_deref());
            }
        }
        Ok(Response::new(resp))
    }

    async fn do_load(
        &self,
        rec: &mut Rec,
        peer: Peer,
        mut req: CasLoadRequest,
    ) -> Result<Response<CasLoadResponse>, Status> {
        // Owned: `req` moves upstream, and the id keys payload reuse on the way back.
        let id = req.cas_id.as_ref().map(|i| i.id.clone()).unwrap_or_default();
        rec.object(&id);
        rec.request = json!({
            "cas_id": hex::encode(&id),
            "write_to_disk": req.write_to_disk,
        });
        let cacheable = valid_id(&id);
        if cacheable {
            if let Some(data) = self.store.get_blob(&id).await.map_err(io)? {
                info!("{peer} down cas load ← local cache {}", human_bytes(data.len() as u64));
                let digest = rec.object_payload("response.blob", &id, &data);
                let blob = blob::respond(data, req.write_to_disk, self.blob_dir.as_deref())
                    .await
                    .map_err(io)?;
                rec.track(&blob);
                let mut described = blob_json(Some(&blob));
                described["payload"] = json!(digest);
                rec.outcome("hit");
                rec.response = json!({ "outcome": "success", "blob": described });
                return Ok(Response::new(CasLoadResponse {
                    outcome: cas_load_response::Outcome::Success as i32,
                    contents: Some(cas_load_response::Contents::Data(CasBlob { blob: Some(blob) })),
                }));
            }
        }
        let want_file = std::mem::replace(&mut req.write_to_disk, false);
        let mut resp = self.upstream.clone().load(req).await?.into_inner();
        if resp.outcome == cas_load_response::Outcome::Success as i32 {
            if let Some(cas_load_response::Contents::Data(b)) = resp.contents.take() {
                let complete = has_payload(&b.blob);
                let mut data = blob::read(b.blob).await.map_err(io)?;
                info!("{peer} down cas load ← upstream {}", human_bytes(data.len() as u64));
                if cacheable && complete {
                    data = self.store.cache_blob(&id, data).await.map_err(io)?;
                }
                let digest = rec.object_payload("response.blob", &id, &data);
                let blob = blob::respond(data, want_file, self.blob_dir.as_deref())
                    .await
                    .map_err(io)?;
                rec.track(&blob);
                let mut described = blob_json(Some(&blob));
                described["payload"] = json!(digest);
                rec.outcome("hit");
                rec.response = json!({ "outcome": "success", "blob": described });
                resp.contents = Some(cas_load_response::Contents::Data(CasBlob { blob: Some(blob) }));
            } else {
                rec.response = json!({ "outcome": "success" });
                upstream_error(rec, Some("upstream success without data"));
            }
        } else {
            info!("{peer} ---  cas load ← upstream miss");
            if resp.outcome == cas_load_response::Outcome::ObjectNotFound as i32 {
                rec.outcome("miss");
                rec.response = json!({ "outcome": "object_not_found" });
            } else {
                let desc = match &resp.contents {
                    Some(cas_load_response::Contents::Error(e)) => Some(e.description.clone()),
                    _ => None,
                };
                rec.response = json!({ "outcome": "error", "error": desc });
                upstream_error(rec, desc.as_deref());
            }
        }
        Ok(Response::new(resp))
    }
}

#[tonic::async_trait]
impl CasdbService for CasProxy {
    async fn put(&self, req: Request<CasPutRequest>) -> Result<Response<CasPutResponse>, Status> {
        let peer = Peer::of(&req);
        let mut rec = self.journal.begin("CAS.Put", "up", peer);
        let res = self.do_put(&mut rec, peer, req.into_inner()).await;
        rec.done(res).await
    }

    async fn save(&self, req: Request<CasSaveRequest>) -> Result<Response<CasSaveResponse>, Status> {
        let peer = Peer::of(&req);
        let mut rec = self.journal.begin("CAS.Save", "up", peer);
        let res = self.do_save(&mut rec, peer, req.into_inner()).await;
        rec.done(res).await
    }

    async fn get(&self, req: Request<CasGetRequest>) -> Result<Response<CasGetResponse>, Status> {
        let peer = Peer::of(&req);
        let mut rec = self.journal.begin("CAS.Get", "down", peer);
        let res = self.do_get(&mut rec, peer, req.into_inner()).await;
        rec.done(res).await
    }

    async fn load(&self, req: Request<CasLoadRequest>) -> Result<Response<CasLoadResponse>, Status> {
        let peer = Peer::of(&req);
        let mut rec = self.journal.begin("CAS.Load", "down", peer);
        let res = self.do_load(&mut rec, peer, req.into_inner()).await;
        rec.done(res).await
    }
}

impl KvProxy {
    async fn do_get(
        &self,
        rec: &mut Rec,
        peer: Peer,
        req: GetValueRequest,
    ) -> Result<Response<GetValueResponse>, Status> {
        rec.object(&req.key);
        rec.request = json!({ "key": hex::encode(&req.key) });
        let key = req.key.clone();
        let cacheable = valid_id(&key);
        if cacheable {
            if let Some(raw) = self.store.get_value(&key).await.map_err(io)? {
                let value = Value::decode(raw.as_slice())
                    .map_err(|e| io(anyhow::Error::new(e).context("decoding cached value")))?;
                info!("{peer} down kv  get  ← local cache entries={}", value.entries.len());
                let entries = entries_json(rec, "response.", &value.entries);
                rec.outcome("hit");
                rec.response = json!({ "outcome": "success", "entries": entries });
                return Ok(Response::new(GetValueResponse {
                    outcome: get_value_response::Outcome::Success as i32,
                    contents: Some(get_value_response::Contents::Value(value)),
                }));
            }
        }
        info!("{peer} ---  kv  get  → upstream");
        let resp = self.upstream.clone().get_value(req).await?;
        let r = resp.get_ref();
        if r.outcome == get_value_response::Outcome::Success as i32 {
            let entries = match &r.contents {
                Some(get_value_response::Contents::Value(v)) => {
                    if cacheable {
                        self.store.put_value(&key, v.encode_to_vec()).await.map_err(io)?;
                    }
                    entries_json(rec, "response.", &v.entries)
                }
                _ => json!([]),
            };
            rec.outcome("hit");
            rec.response = json!({ "outcome": "success", "entries": entries });
        } else if r.outcome == get_value_response::Outcome::KeyNotFound as i32 {
            rec.outcome("miss");
            rec.response = json!({ "outcome": "key_not_found" });
        } else {
            let desc = match &r.contents {
                Some(get_value_response::Contents::Error(e)) => Some(e.description.clone()),
                _ => None,
            };
            rec.response = json!({ "outcome": "error", "error": desc });
            upstream_error(rec, desc.as_deref());
        }
        Ok(resp)
    }

    async fn do_put(
        &self,
        rec: &mut Rec,
        peer: Peer,
        req: PutValueRequest,
    ) -> Result<Response<PutValueResponse>, Status> {
        rec.object(&req.key);
        let entries = match &req.value {
            Some(v) => entries_json(rec, "request.", &v.entries),
            None => json!([]),
        };
        rec.request = json!({ "key": hex::encode(&req.key), "entries": entries });
        let key = req.key.clone();
        let encoded = valid_id(&key).then(|| req.value.as_ref().map(|v| v.encode_to_vec()).unwrap_or_default());
        info!("{peer} up   kv  put  → upstream");
        let resp = self.upstream.clone().put_value(req).await?;
        match &resp.get_ref().error {
            Some(e) => {
                rec.response = json!({ "error": e.description });
                upstream_error(rec, Some(&e.description));
            }
            None => {
                if let Some(encoded) = encoded {
                    self.store.put_value(&key, encoded).await.map_err(io)?;
                }
                rec.response = json!({ "error": null });
            }
        }
        Ok(resp)
    }
}

#[tonic::async_trait]
impl KeyValueDb for KvProxy {
    async fn get_value(&self, req: Request<GetValueRequest>) -> Result<Response<GetValueResponse>, Status> {
        let peer = Peer::of(&req);
        let mut rec = self.journal.begin("KV.GetValue", "down", peer);
        let res = self.do_get(&mut rec, peer, req.into_inner()).await;
        rec.done(res).await
    }

    async fn put_value(&self, req: Request<PutValueRequest>) -> Result<Response<PutValueResponse>, Status> {
        let peer = Peer::of(&req);
        let mut rec = self.journal.begin("KV.PutValue", "up", peer);
        let res = self.do_put(&mut rec, peer, req.into_inner()).await;
        rec.done(res).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};

    use parking_lot::Mutex;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio::task::JoinHandle;
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::transport::{Endpoint, Server};

    use super::*;
    use crate::hash::hash_object;
    use crate::journal::Journal;
    use crate::proto::cas::casdb_service_server::CasdbServiceServer;
    use crate::proto::cas::ResponseError;
    use crate::proto::kv::key_value_db_server::KeyValueDbServer;
    use crate::service::{CasService, KvService};

    /// An upstream that is a real tonic server on a loopback port.
    struct Upstream {
        addr: SocketAddr,
        stop: Option<oneshot::Sender<()>>,
        task: Option<JoinHandle<()>>,
    }

    impl Upstream {
        async fn start(cas: impl CasdbService, kv: impl KeyValueDb) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (stop, stopped) = oneshot::channel::<()>();
            let task = tokio::spawn(async move {
                Server::builder()
                    .add_service(CasdbServiceServer::new(cas))
                    .add_service(KeyValueDbServer::new(kv))
                    .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                        let _ = stopped.await;
                    })
                    .await
                    .unwrap();
            });
            Upstream {
                addr,
                stop: Some(stop),
                task: Some(task),
            }
        }

        async fn stop(&mut self) {
            let _ = self.stop.take().unwrap().send(());
            self.task.take().unwrap().await.unwrap();
        }
    }

    /// A real upstream (`CasService`/`KvService` over its own store) plus a
    /// proxy in front of it with a separate cache store.
    struct Rig {
        root: PathBuf,
        upstream_store: Arc<Store>,
        upstream: Upstream,
        cas: CasProxy,
        kv: KvProxy,
    }

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("swiftcache-proxy-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn proxies(root: &Path, addr: SocketAddr) -> (CasProxy, KvProxy) {
        let journal = Arc::new(Journal::open(&root.join("journal")).unwrap());
        let store = Arc::new(Store::open(&root.join("proxy")).await.unwrap());
        let channel = Endpoint::from_shared(format!("http://{addr}")).unwrap().connect_lazy();
        (
            CasProxy {
                upstream: CasdbServiceClient::new(channel.clone()),
                blob_dir: None,
                journal: journal.recorder(Some("proxy")),
                store: store.clone(),
            },
            KvProxy {
                upstream: KeyValueDbClient::new(channel),
                journal: journal.recorder(Some("proxy")),
                store,
            },
        )
    }

    async fn rig(name: &str) -> Rig {
        let root = temp_root(name);
        let journal = Arc::new(Journal::open(&root.join("up-journal")).unwrap());
        let upstream_store = Arc::new(Store::open(&root.join("upstream")).await.unwrap());
        let upstream = Upstream::start(
            CasService {
                store: upstream_store.clone(),
                blob_dir: None,
                journal: journal.recorder(None),
            },
            KvService {
                store: upstream_store.clone(),
                journal: journal.recorder(None),
            },
        )
        .await;
        let (cas, kv) = proxies(&root, upstream.addr).await;
        Rig {
            root,
            upstream_store,
            upstream,
            cas,
            kv,
        }
    }

    fn bytes(data: &[u8]) -> Option<CasBytes> {
        Some(CasBytes {
            contents: Some(cas_bytes::Contents::Data(data.to_vec())),
        })
    }

    fn put_request(refs: &[Vec<u8>], data: &[u8]) -> Request<CasPutRequest> {
        Request::new(CasPutRequest {
            data: Some(CasObject {
                blob: bytes(data),
                references: refs.iter().cloned().map(|id| CasDataId { id }).collect(),
            }),
        })
    }

    fn get_request(id: &[u8]) -> Request<CasGetRequest> {
        Request::new(CasGetRequest {
            cas_id: Some(CasDataId { id: id.to_vec() }),
            write_to_disk: false,
        })
    }

    fn load_request(id: &[u8]) -> Request<CasLoadRequest> {
        Request::new(CasLoadRequest {
            cas_id: Some(CasDataId { id: id.to_vec() }),
            write_to_disk: false,
        })
    }

    fn inline_data(blob: Option<CasBytes>) -> Vec<u8> {
        match blob.and_then(|b| b.contents) {
            Some(cas_bytes::Contents::Data(d)) => d,
            other => panic!("expected inline data, got {other:?}"),
        }
    }

    /// `(data, references)` of a successful `CAS.Get`.
    fn object_of(resp: CasGetResponse) -> (Vec<u8>, Vec<Vec<u8>>) {
        assert_eq!(resp.outcome, cas_get_response::Outcome::Success as i32);
        let Some(cas_get_response::Contents::Data(obj)) = resp.contents else {
            panic!("get without data")
        };
        (
            inline_data(obj.blob),
            obj.references.into_iter().map(|r| r.id).collect(),
        )
    }

    fn loaded(resp: CasLoadResponse) -> Vec<u8> {
        assert_eq!(resp.outcome, cas_load_response::Outcome::Success as i32);
        let Some(cas_load_response::Contents::Data(b)) = resp.contents else {
            panic!("load without data")
        };
        inline_data(b.blob)
    }

    fn put_id(resp: CasPutResponse) -> Vec<u8> {
        match resp.contents {
            Some(cas_put_response::Contents::CasId(id)) => id.id,
            other => panic!("put failed: {other:?}"),
        }
    }

    fn staging_files(root: &Path) -> usize {
        std::fs::read_dir(root.join("proxy").join("tmp")).unwrap().count()
    }

    /// Objects read through the proxy are served locally afterwards, with
    /// their references, even when the upstream is gone; a miss is never
    /// remembered.
    #[tokio::test(flavor = "multi_thread")]
    async fn cold_read_populates_cache_that_outlives_upstream() {
        let mut rig = rig("cold").await;
        let leaf = rig.upstream_store.put_object(vec![], b"leaf".to_vec()).await.unwrap();
        let id = rig
            .upstream_store
            .put_object(vec![leaf.clone()], b"payload".to_vec())
            .await
            .unwrap();

        // A miss followed by the object appearing upstream must hit.
        let late = hash_object(&[], b"late").to_vec();
        let miss = rig.cas.get(get_request(&late)).await.unwrap().into_inner();
        assert_eq!(miss.outcome, cas_get_response::Outcome::ObjectNotFound as i32);
        rig.upstream_store.put_object(vec![], b"late".to_vec()).await.unwrap();
        let (data, refs) = object_of(rig.cas.get(get_request(&late)).await.unwrap().into_inner());
        assert_eq!((data, refs), (b"late".to_vec(), vec![]));

        let (data, refs) = object_of(rig.cas.get(get_request(&id)).await.unwrap().into_inner());
        assert_eq!((data.as_slice(), refs), (&b"payload"[..], vec![leaf.clone()]));

        rig.upstream.stop().await;
        let (data, refs) = object_of(rig.cas.get(get_request(&id)).await.unwrap().into_inner());
        assert_eq!((data.as_slice(), refs), (&b"payload"[..], vec![leaf]));
        assert_eq!(
            loaded(rig.cas.load(load_request(&id)).await.unwrap().into_inner()),
            b"payload"
        );
        let (data, _) = object_of(rig.cas.get(get_request(&late)).await.unwrap().into_inner());
        assert_eq!(data, b"late");
        // The upstream really is down: an uncached object is an error, not a miss.
        assert!(rig.cas.get(get_request(&[3u8; 32])).await.is_err());
    }

    /// `CAS.Load` carries no references, so what it caches must not answer a
    /// later `CAS.Get` (which would silently drop them).
    #[tokio::test(flavor = "multi_thread")]
    async fn load_result_never_satisfies_get() {
        let mut rig = rig("load").await;
        let leaf = rig.upstream_store.put_object(vec![], b"leaf".to_vec()).await.unwrap();
        let id = rig
            .upstream_store
            .put_object(vec![leaf.clone()], b"payload".to_vec())
            .await
            .unwrap();

        assert_eq!(
            loaded(rig.cas.load(load_request(&id)).await.unwrap().into_inner()),
            b"payload"
        );
        rig.upstream.stop().await;
        assert_eq!(
            loaded(rig.cas.load(load_request(&id)).await.unwrap().into_inner()),
            b"payload"
        );
        assert!(
            rig.cas.get(get_request(&id)).await.is_err(),
            "Get must go upstream, not reuse the blob"
        );
    }

    /// Uploads are written through and are local hits afterwards, with the
    /// upstream-assigned id and the references of the upload.
    #[tokio::test(flavor = "multi_thread")]
    async fn uploads_are_cached_under_upstream_id() {
        let mut rig = rig("upload").await;
        let saved = rig
            .cas
            .save(Request::new(CasSaveRequest {
                data: Some(CasBlob { blob: bytes(b"leaf") }),
            }))
            .await
            .unwrap()
            .into_inner();
        let leaf = match saved.contents {
            Some(cas_save_response::Contents::CasId(id)) => id.id,
            other => panic!("save failed: {other:?}"),
        };
        let id = put_id(
            rig.cas
                .put(put_request(std::slice::from_ref(&leaf), b"payload"))
                .await
                .unwrap()
                .into_inner(),
        );
        assert_eq!(id, hash_object(std::slice::from_ref(&leaf), b"payload"));
        assert_eq!(staging_files(&rig.root), 0);
        // The upstream received the same objects.
        assert!(rig.upstream_store.get_object(&id).await.unwrap().is_some());

        rig.upstream.stop().await;
        let (data, refs) = object_of(rig.cas.get(get_request(&id)).await.unwrap().into_inner());
        assert_eq!((data.as_slice(), refs), (&b"payload"[..], vec![leaf.clone()]));
        let (data, refs) = object_of(rig.cas.get(get_request(&leaf)).await.unwrap().into_inner());
        assert_eq!((data.as_slice(), refs), (&b"leaf"[..], vec![]));
    }

    /// Stored object: references and data, keyed by its opaque id.
    type OpaqueObjects = HashMap<Vec<u8>, (Vec<Vec<u8>>, Vec<u8>)>;

    /// Toolchain ids are opaque (65-byte `luxon.blake2b` digests); they must
    /// be kept exactly, not re-derived locally.
    struct Opaque {
        fail_puts: Arc<AtomicBool>,
        objects: Mutex<OpaqueObjects>,
    }

    fn opaque_id(data: &[u8]) -> Vec<u8> {
        let mut id = vec![0xA5; 65];
        id[0] = data.len() as u8;
        id
    }

    #[tonic::async_trait]
    impl CasdbService for Opaque {
        async fn put(&self, req: Request<CasPutRequest>) -> Result<Response<CasPutResponse>, Status> {
            if self.fail_puts.load(Ordering::SeqCst) {
                let error = ResponseError {
                    description: "disk full".into(),
                };
                return Ok(Response::new(CasPutResponse {
                    contents: Some(cas_put_response::Contents::Error(error)),
                }));
            }
            let obj = req.into_inner().data.unwrap();
            let data = inline_data(obj.blob);
            let id = opaque_id(&data);
            let refs = obj.references.into_iter().map(|r| r.id).collect();
            self.objects.lock().insert(id.clone(), (refs, data));
            Ok(Response::new(CasPutResponse {
                contents: Some(cas_put_response::Contents::CasId(CasDataId { id })),
            }))
        }

        async fn get(&self, req: Request<CasGetRequest>) -> Result<Response<CasGetResponse>, Status> {
            let id = req.into_inner().cas_id.unwrap().id;
            Ok(Response::new(match self.objects.lock().get(&id) {
                Some((refs, data)) => CasGetResponse {
                    outcome: cas_get_response::Outcome::Success as i32,
                    contents: Some(cas_get_response::Contents::Data(CasObject {
                        blob: bytes(data),
                        references: refs.iter().cloned().map(|id| CasDataId { id }).collect(),
                    })),
                },
                None => CasGetResponse {
                    outcome: cas_get_response::Outcome::ObjectNotFound as i32,
                    contents: None,
                },
            }))
        }

        async fn save(&self, _: Request<CasSaveRequest>) -> Result<Response<CasSaveResponse>, Status> {
            Err(Status::unimplemented("not used"))
        }

        async fn load(&self, _: Request<CasLoadRequest>) -> Result<Response<CasLoadResponse>, Status> {
            Err(Status::unimplemented("not used"))
        }
    }

    async fn opaque_rig(name: &str) -> (PathBuf, Upstream, Arc<AtomicBool>, CasProxy) {
        let root = temp_root(name);
        let journal = Arc::new(Journal::open(&root.join("up-journal")).unwrap());
        let kv_store = Arc::new(Store::open(&root.join("upstream")).await.unwrap());
        let fail_puts = Arc::new(AtomicBool::new(false));
        let upstream = Upstream::start(
            Opaque {
                fail_puts: fail_puts.clone(),
                objects: Mutex::new(HashMap::new()),
            },
            KvService {
                store: kv_store,
                journal: journal.recorder(None),
            },
        )
        .await;
        let (cas, _) = proxies(&root, upstream.addr).await;
        (root, upstream, fail_puts, cas)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn opaque_ids_and_references_are_preserved() {
        let (_root, mut upstream, _, cas) = opaque_rig("opaque").await;
        let refs = vec![vec![9u8; 65], vec![1u8; 80]];
        let id = put_id(cas.put(put_request(&refs, b"x")).await.unwrap().into_inner());
        assert_eq!(id, opaque_id(b"x"));

        upstream.stop().await;
        let (data, got) = object_of(cas.get(get_request(&id)).await.unwrap().into_inner());
        assert_eq!((data.as_slice(), got), (&b"x"[..], refs));
    }

    /// An upload the upstream did not accept must leave nothing behind and
    /// must not turn into a local hit.
    #[tokio::test(flavor = "multi_thread")]
    async fn rejected_or_failed_uploads_leave_no_cache_entry() {
        let (root, mut upstream, fail_puts, cas) = opaque_rig("rejected").await;

        fail_puts.store(true, Ordering::SeqCst);
        let resp = cas.put(put_request(&[], b"x")).await.unwrap().into_inner();
        assert!(matches!(resp.contents, Some(cas_put_response::Contents::Error(_))));
        assert_eq!(staging_files(&root), 0);
        let miss = cas.get(get_request(&opaque_id(b"x"))).await.unwrap().into_inner();
        assert_eq!(miss.outcome, cas_get_response::Outcome::ObjectNotFound as i32);

        upstream.stop().await;
        assert!(cas.put(put_request(&[], b"y")).await.is_err());
        assert_eq!(staging_files(&root), 0);
        assert!(
            cas.get(get_request(&opaque_id(b"y"))).await.is_err(),
            "failed upload must not be a local hit"
        );
    }

    fn value(k: &str, v: &[u8]) -> Value {
        Value {
            entries: HashMap::from([(k.to_string(), v.to_vec())]),
        }
    }

    fn entries_of(resp: GetValueResponse) -> HashMap<String, Vec<u8>> {
        assert_eq!(resp.outcome, get_value_response::Outcome::Success as i32);
        let Some(get_value_response::Contents::Value(v)) = resp.contents else {
            panic!("get without value")
        };
        v.entries
    }

    /// The action cache is cached on both paths, and a missing key is not.
    #[tokio::test(flavor = "multi_thread")]
    async fn action_cache_is_read_through_and_write_through() {
        let mut rig = rig("kv").await;
        let key = b"swift-key".to_vec();
        let miss = rig
            .kv
            .get_value(Request::new(GetValueRequest { key: key.clone() }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(miss.outcome, get_value_response::Outcome::KeyNotFound as i32);

        rig.upstream_store
            .put_value(&key, value("a", b"1").encode_to_vec())
            .await
            .unwrap();
        let got = rig
            .kv
            .get_value(Request::new(GetValueRequest { key: key.clone() }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(entries_of(got), value("a", b"1").entries);

        let written = b"written".to_vec();
        let put = PutValueRequest {
            key: written.clone(),
            value: Some(value("b", b"2")),
        };
        assert!(rig
            .kv
            .put_value(Request::new(put))
            .await
            .unwrap()
            .into_inner()
            .error
            .is_none());
        assert!(rig.upstream_store.get_value(&written).await.unwrap().is_some());

        rig.upstream.stop().await;
        for (key, want) in [(key, value("a", b"1")), (written, value("b", b"2"))] {
            let got = rig
                .kv
                .get_value(Request::new(GetValueRequest { key }))
                .await
                .unwrap()
                .into_inner();
            assert_eq!(entries_of(got), want.entries);
        }
        let put = PutValueRequest {
            key: b"never".to_vec(),
            value: Some(value("c", b"3")),
        };
        assert!(rig.kv.put_value(Request::new(put)).await.is_err());
        let key = b"never".to_vec();
        assert!(
            rig.kv.get_value(Request::new(GetValueRequest { key })).await.is_err(),
            "failed put must not be a local hit"
        );
    }
}
