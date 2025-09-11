use std::fmt::Display;

use openraft::error::Infallible;
use openraft::error::InstallSnapshotError;
use openraft::error::NetworkError;
use openraft::error::RPCError;
use openraft::error::RaftError;
use openraft::error::RemoteError;
use openraft::error::Unreachable;
use openraft::network::RPCOption;
use openraft::network::RaftNetwork;
use openraft::network::RaftNetworkFactory;
use openraft::raft::AppendEntriesRequest;
use openraft::raft::AppendEntriesResponse;
use openraft::raft::InstallSnapshotRequest;
use openraft::raft::InstallSnapshotResponse;
use openraft::raft::VoteRequest;
use openraft::raft::VoteResponse;
use openraft::BasicNode;
use openraft::RaftTypeConfig;
use serde_json::Value as JsonValue;
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LinesCodec};
use futures::{SinkExt, StreamExt};
use futures::stream::SplitSink;
use tokio::sync::{Mutex, mpsc, oneshot};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::AsyncRead;
use tokio::io::AsyncSeek;
use tokio::io::AsyncWrite;

pub struct NetworkFactory {
    host_socket: String,
    // lazily-initialized shared host connection manager
    host_conn: Option<Arc<HostConnection>>,
    // event channel to receive unsolicited events from host
    event_tx: mpsc::UnboundedSender<JsonValue>,
    // keep the receiver so the caller can take it before the factory is moved
    event_rx: Option<mpsc::UnboundedReceiver<JsonValue>>,
}

impl NetworkFactory {
    pub fn new(host_socket: String) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self { host_socket, host_conn: None, event_tx: tx, event_rx: Some(rx) }
    }
}

impl NetworkFactory {
    /// Take the event receiver. Call this before moving the factory into the Raft runtime.
    pub fn take_event_receiver(&mut self) -> Option<mpsc::UnboundedReceiver<JsonValue>> {
        self.event_rx.take()
    }
}

/// Manages a persistent connection to the host agent over a unix socket.
pub struct HostConnection {
    writer: Mutex<SplitSink<Framed<UnixStream, LinesCodec>, String>>,
    pending: Mutex<HashMap<String, oneshot::Sender<JsonValue>>>,
    id_counter: AtomicU64,
    event_tx: mpsc::UnboundedSender<JsonValue>,
}

impl HostConnection {
    /// Connects to the socket and spawns a reader task. Returns the manager and the event receiver.
    pub async fn connect(
        socket_path: String,
        event_tx: mpsc::UnboundedSender<JsonValue>,
    ) -> Result<Arc<Self>, std::io::Error> {
        let stream = UnixStream::connect(&socket_path).await?;
        let framed = Framed::new(stream, LinesCodec::new());
    let (sink, stream) = framed.split();

        let host = Arc::new(HostConnection {
            writer: Mutex::new(sink),
            pending: Mutex::new(HashMap::new()),
            id_counter: AtomicU64::new(1),
            event_tx: event_tx.clone(),
        });

        // spawn reader task with reconnect/backoff logic
        let h = host.clone();
        tokio::spawn(async move {
            // `stream` is the framed read half; we will replace it on reconnect.
            let mut stream = stream;

            loop {
                let mut disconnected = false;

                while let Some(item) = stream.next().await {
                    match item {
                        Ok(line) => {
                            if line.is_empty() {
                                continue;
                            }
                            match serde_json::from_str::<JsonValue>(&line) {
                                Ok(v) => {
                                    if let Some(id) = v.get("id").and_then(|x| x.as_str()) {
                                        let mut pending = h.pending.lock().await;
                                        if let Some(tx) = pending.remove(id) {
                                            // reply to an outstanding gateway request
                                            let _ = tx.send(v);
                                            continue;
                                        } else {
                                            // host-initiated request: check uri and reply if handled
                                            if let Some(uri) = v.get("uri").and_then(|x| x.as_str()) {
                                                match uri {
                                                    "health" => {
                                                        // reply with simple JSON OK using same id
                                                        let resp = serde_json::json!({"id": id, "ok": true});
                                                        let s = resp.to_string();
                                                        let mut w = h.writer.lock().await;
                                                        let _ = w.send(s).await;
                                                        continue;
                                                    }
                                                    _ => {
                                                        // unknown host request: forward to event channel for app handling
                                                        let _ = h.event_tx.send(v);
                                                        continue;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    let _ = h.event_tx.send(v);
                                }
                                Err(_) => {
                                    // ignore parse errors for now
                                }
                            }
                        }
                        Err(_) => {
                            disconnected = true;
                            break;
                        }
                    }
                }

                if !disconnected {
                    // stream ended normally (None), treat as disconnection
                    disconnected = true;
                }

                if disconnected {
                    // Attempt to reconnect with exponential backoff a few times.
                    const MAX_RETRIES: usize = 5;
                    let mut attempt = 0usize;
                    let mut reconnected = false;

                    while attempt < MAX_RETRIES {
                        attempt += 1;
                        let wait_ms = 100u64 * (1 << (attempt - 1));
                        tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
                        match UnixStream::connect(&socket_path).await {
                            Ok(new_stream) => {
                                let framed = Framed::new(new_stream, LinesCodec::new());
                                let (new_sink, new_stream) = framed.split();

                                // replace writer sink so senders start using new connection
                                {
                                    let mut w = h.writer.lock().await;
                                    // replace the old sink with the new one
                                    drop(std::mem::replace(&mut *w, new_sink));
                                }

                                // set the stream to the newly connected stream and resume reading
                                stream = new_stream;
                                reconnected = true;
                                break;
                            }
                            Err(_) => {
                                // try again
                            }
                        }
                    }

                    if reconnected {
                        continue; // outer loop will resume reading from updated `stream`
                    }

                    // failed to reconnect: clear pending so receivers see cancellation and exit
                    let mut pending = h.pending.lock().await;
                    pending.clear();
                    break;
                }
            }
        });

        Ok(host)
    }

    /// Send a JSON payload and await a JSON reply with matching `id`.
    pub async fn send_request(&self, mut payload: JsonValue) -> Result<JsonValue, std::io::Error> {
        let id = self.id_counter.fetch_add(1, Ordering::SeqCst).to_string();
        payload["id"] = JsonValue::String(id.clone());

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(id.clone(), tx);
        }

        let s = payload.to_string();
        // write the line
        {
            let mut w = self.writer.lock().await;
            w.send(s).await.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("send error: {}", e)))?;
        }

        // wait for response
        match rx.await {
            Ok(v) => Ok(v),
            Err(_) => Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "connection closed")),
        }
    }
}

impl<C> RaftNetworkFactory<C> for NetworkFactory
where
    C: RaftTypeConfig<Node = BasicNode>,
    // RaftNetworkV2 is implemented automatically for RaftNetwork, but requires the following trait bounds.
    // In V2 network, the snapshot has no constraints, but RaftNetwork assumes a Snapshot is a file-like
    // object that can be seeked, read from, and written to.
    <C as RaftTypeConfig>::SnapshotData: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
{
    type Network = Network<C>;

    #[tracing::instrument(level = "debug", skip_all)]
    async fn new_client(&mut self, target: C::NodeId, node: &BasicNode) -> Self::Network {
        let addr = node.addr.clone();

        // ensure host connection exists
        if self.host_conn.is_none() {
            if let Ok(conn) = HostConnection::connect(self.host_socket.clone(), self.event_tx.clone()).await {
                self.host_conn = Some(conn);
            }
        }

        let host_conn = self.host_conn.clone().unwrap();

        Network { addr, target, host_conn }
    }
}

pub struct Network<C>
where C: RaftTypeConfig
{
    addr: String,
    target: C::NodeId,
    host_conn: Arc<HostConnection>,
}

impl<C> Network<C>
where C: RaftTypeConfig
{
    async fn request<Req, Resp, Err>(&mut self, uri: impl Display, req: Req) -> Result<Result<Resp, Err>, RPCError<C>>
    where
        Req: Serialize + 'static,
        Resp: Serialize + DeserializeOwned,
        Err: std::error::Error + Serialize + DeserializeOwned,
    {
        // Encode request as JSON object with target, uri and body, send over host unix socket
        let payload = serde_json::json!({
            "target": self.target.to_string(),
            "addr": self.addr.clone(),
            "uri": uri.to_string(),
            "body": serde_json::to_value(&req).map_err(|e| RPCError::Network(NetworkError::new(&e)))?,
        });

        // Use shared host connection manager to send request and await reply
        match self.host_conn.send_request(payload).await {
            Ok(v) => {
                let res: Result<Resp, Err> = serde_json::from_value(v).map_err(|e| NetworkError::new(&e))?;
                Ok(res)
            }
            Err(e) => Err(RPCError::Unreachable(Unreachable::new(&e))),
        }
    }
}

#[allow(clippy::blocks_in_conditions)]
impl<C> RaftNetwork<C> for Network<C>
where C: RaftTypeConfig
{
    #[tracing::instrument(level = "debug", skip_all, err(Debug))]
    async fn append_entries(
        &mut self,
        req: AppendEntriesRequest<C>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<C>, RPCError<C, RaftError<C>>> {
        let res = self.request::<_, _, Infallible>("append", req).await.map_err(RPCError::with_raft_error)?;
        Ok(res.unwrap())
    }

    #[tracing::instrument(level = "debug", skip_all, err(Debug))]
    async fn install_snapshot(
        &mut self,
        req: InstallSnapshotRequest<C>,
        _option: RPCOption,
    ) -> Result<InstallSnapshotResponse<C>, RPCError<C, RaftError<C, InstallSnapshotError>>> {
        let res = self.request("snapshot", req).await.map_err(RPCError::with_raft_error)?;
        match res {
            Ok(resp) => Ok(resp),
            Err(e) => Err(RPCError::RemoteError(RemoteError::new(
                self.target.clone(),
                RaftError::APIError(e),
            ))),
        }
    }

    #[tracing::instrument(level = "debug", skip_all, err(Debug))]
    async fn vote(
        &mut self,
        req: VoteRequest<C>,
        _option: RPCOption,
    ) -> Result<VoteResponse<C>, RPCError<C, RaftError<C>>> {
        let res = self.request::<_, _, Infallible>("vote", req).await.map_err(RPCError::with_raft_error)?;
        Ok(res.unwrap())
    }
}