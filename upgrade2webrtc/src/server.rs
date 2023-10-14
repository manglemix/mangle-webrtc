use std::{fmt::Display, future::Future, sync::Arc};

use async_trait::async_trait;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, ToSocketAddrs},
    sync::mpsc,
};
use tokio_rustls::TlsAcceptor;
use webrtc::{
    api::{
        interceptor_registry::register_default_interceptors, media_engine::MediaEngine, APIBuilder,
        API,
    },
    data_channel::RTCDataChannel,
    ice_transport::{ice_candidate::RTCIceCandidate, ice_server::RTCIceServer},
    interceptor::registry::Registry,
    peer_connection::{
        configuration::RTCConfiguration, sdp::session_description::RTCSessionDescription,
        RTCPeerConnection,
    },
};

use crate::{
    tls::{new_tls_acceptor, TlsInitError, TlsServerConfig},
    transport::{
        IDUpgradeTransport, RecvError, StreamTransport, UpgradeTransport, UpgradeTransportServer,
    },
    RTCMessage,
};

pub struct ServerHandle(mpsc::Sender<()>);

impl ServerHandle {
    pub fn close_server(&self) {
        let _ = self.0.try_send(());
    }
}

pub struct UpgradeWebRTCServer<S: UpgradeTransportServer> {
    server: S,
    api: Arc<API>,
    config: RTCConfiguration,
}

#[derive(Debug)]
pub enum ServerError<AE, DE> {
    AcceptError(AE),
    BadMessage { error: DE, client_id: String },
    IOError(std::io::Error),
    WebRTCError(webrtc::Error),
}

impl<AE: Display, DE: Display> Display for ServerError<AE, DE> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServerError::AcceptError(e) => write!(f, "{e}"),
            ServerError::BadMessage { error, client_id } => write!(f, "From: {client_id}\n{error}"),
            ServerError::IOError(e) => write!(f, "{e}"),
            ServerError::WebRTCError(e) => write!(f, "{e}"),
        }
    }
}

impl<S: UpgradeTransportServer> UpgradeWebRTCServer<S> {
    pub fn new(server: S) -> Self {
        let mut m = MediaEngine::default();
        m.register_default_codecs()
            .expect("Default codecs should have registered safely");

        let mut registry = Registry::new();

        // Use the default set of Interceptors
        registry = register_default_interceptors(registry, &mut m)
            .expect("Default interceptors should have registered safely");

        Self {
            server,
            api: Arc::new(
                APIBuilder::new()
                    .with_media_engine(m)
                    .with_interceptor_registry(registry)
                    .build(),
            ),
            config: RTCConfiguration {
                ice_servers: vec![RTCIceServer {
                    urls: vec!["stun:stun.l.google.com:19302".to_owned()],
                    ..Default::default()
                }],
                ..Default::default()
            },
        }
    }

    pub async fn run<F1, F2>(
        &mut self,
        on_success: impl Fn(RTCPeerConnection, mpsc::Receiver<Arc<RTCDataChannel>>, ServerHandle) -> F1
            + Send
            + Sync
            + 'static,
        on_err: impl Fn(
                ServerError<
                    S::AcceptError,
                    <S::UpgradeTransport as UpgradeTransport>::DeserializationError,
                >,
                ServerHandle,
            ) -> F2
            + Send
            + Sync
            + 'static,
    ) where
        F1: Future<Output = ()> + Send,
        F2: Future<Output = ()> + Send,
    {
        let on_success = Arc::new(on_success);
        let on_err = Arc::new(on_err);
        let mut running_tasks = vec![];
        let (close_sender, mut close_recv) = mpsc::channel(1);

        loop {
            tokio::select! {
                result = self.server.accept() => {
                    let mut transport = match result {
                        Ok(x) => x,
                        Err(e) => {
                            on_err(ServerError::AcceptError(e), ServerHandle(close_sender.clone())).await;
                            continue
                        }
                    };

                    let api = self.api.clone();
                    let config = self.config.clone();
                    let on_err = on_err.clone();
                    let on_success = on_success.clone();
                    let close_sender = close_sender.clone();

                    running_tasks.push(tokio::spawn(async move {
                        loop {
                            let offer: RTCSessionDescription = match transport.recv_obj().await {
                                Ok(x) => x,
                                Err(RecvError::DeserializeError(error)) => {
                                    on_err(ServerError::<S::AcceptError, _>::BadMessage {
                                        error,
                                        client_id: transport.get_id()
                                    }, ServerHandle(close_sender)).await;
                                    return
                                }
                                Err(RecvError::IOError(e)) => {
                                    on_err(ServerError::IOError(e), ServerHandle(close_sender)).await;
                                    return
                                }
                            };

                            let peer = match api.new_peer_connection(config.clone()).await {
                                Ok(x) => x,
                                Err(e) => return on_err(ServerError::WebRTCError(e), ServerHandle(close_sender)).await
                            };

                            macro_rules! returning {
                                ($val: expr) => {{
                                    let _ = peer.close().await;
                                    return $val
                                }}
                            }

                            macro_rules! webrtc_unwrap {
                                ($result: expr) => {
                                    match $result {
                                        Ok(x) => x,
                                        Err(e) => returning!(on_err(ServerError::WebRTCError(e), ServerHandle(close_sender)).await)
                                    }
                                };
                            }

                            let (channel_sender, channel_recv) = mpsc::channel(1);
                            let channel_sender = Arc::new(channel_sender);

                            peer.on_data_channel(Box::new(move |data_channel| {
                                let channel_sender = channel_sender.clone();
                                Box::pin(async move { let _ = channel_sender.send(data_channel).await; })
                            }));

                            let (ice_sender, mut ice_receiver) = mpsc::channel(3);
                            let ice_sender = Arc::new(ice_sender);

                            peer.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
                                let ice_sender = ice_sender.clone();
                                Box::pin(async move { let _ = ice_sender.send(c).await; })
                            }));

                            webrtc_unwrap!(peer.set_remote_description(offer).await);
                            let answer = RTCMessage::SDPAnswer(webrtc_unwrap!(peer.create_answer(None).await));
                            if let Err(e) = transport.send_obj(&answer).await {
                                returning!(on_err(ServerError::IOError(e), ServerHandle(close_sender)).await)
                            }
                            let RTCMessage::SDPAnswer(answer) = answer else { unreachable!() };
                            webrtc_unwrap!(peer.set_local_description(answer).await);

                            let mut done_sending_ice = false;
                            let mut done_receiving_ice = false;

                            loop {
                                tokio::select! {
                                    ice_to_send = ice_receiver.recv() => {
                                        let ice_to_send = ice_to_send.unwrap();
                                        let ice_to_send = webrtc_unwrap!(ice_to_send.map(|x| x.to_json()).transpose());
                                        let ice_to_send = ice_to_send.map(RTCMessage::ICE);
                                        if let Err(e) = transport.send_obj(&ice_to_send).await {
                                            returning!(on_err(ServerError::IOError(e), ServerHandle(close_sender)).await);
                                        }
                                        if ice_to_send.is_none() {
                                            done_sending_ice = true;
                                            if done_receiving_ice {
                                                break
                                            }
                                        };
                                    }
                                    received_ice = transport.recv_obj::<Option<RTCIceCandidate>>() => {
                                        let received_ice = match received_ice {
                                            Ok(x) => x,
                                            Err(RecvError::DeserializeError(error)) => returning!(on_err(ServerError::BadMessage { error, client_id: transport.get_id() }, ServerHandle(close_sender)).await),
                                            Err(RecvError::IOError(e)) => returning!(on_err(ServerError::IOError(e), ServerHandle(close_sender)).await)
                                        };
                                        let Some(received_ice) = received_ice else {
                                            done_receiving_ice = true;
                                            if done_sending_ice {
                                                break
                                            }
                                            continue
                                        };
                                        let received_ice = webrtc_unwrap!(received_ice.to_json());
                                        webrtc_unwrap!(peer.add_ice_candidate(received_ice).await);
                                    }
                                }
                            }

                            on_success(peer, channel_recv, ServerHandle(close_sender.clone())).await;
                        }
                    }));
                }
                _ = close_recv.recv() => {
                    break
                }
            }
        }

        for handle in &running_tasks {
            handle.abort();
        }

        for handle in running_tasks {
            let _ = handle.await;
        }
    }
}

pub struct TlsUpgradeTransportServer<S: UpgradeTransportServer> {
    server: S,
    acceptor: TlsAcceptor,
}

#[derive(Debug)]
pub enum TlsAcceptError<E> {
    AcceptError(E),
    IOError(std::io::Error),
}

impl<E: std::fmt::Debug + std::fmt::Display> std::error::Error for TlsAcceptError<E> {}

impl<E: Display> Display for TlsAcceptError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TlsAcceptError::AcceptError(e) => write!(f, "{e}"),
            TlsAcceptError::IOError(e) => write!(f, "{e}"),
        }
    }
}

impl<E> From<std::io::Error> for TlsAcceptError<E> {
    fn from(value: std::io::Error) -> Self {
        Self::IOError(value)
    }
}

#[async_trait]
impl<C, S> UpgradeTransportServer for TlsUpgradeTransportServer<S>
where
    C: AsyncWrite + AsyncRead + Send + Sync + Unpin + 'static + IDUpgradeTransport,
    S: UpgradeTransportServer<UpgradeTransport = StreamTransport<C>>,
{
    type AcceptError = TlsAcceptError<S::AcceptError>;

    type UpgradeTransport = StreamTransport<tokio_rustls::server::TlsStream<C>>;

    async fn accept(&mut self) -> Result<Self::UpgradeTransport, Self::AcceptError> {
        let stream = self
            .acceptor
            .accept(
                self.server
                    .accept()
                    .await
                    .map_err(|e| TlsAcceptError::AcceptError(e))?
                    .stream,
            )
            .await?;
        Ok(StreamTransport::from(stream))
    }
}

impl<C, S> UpgradeWebRTCServer<S>
where
    C: AsyncWrite + AsyncRead + Send + Sync + Unpin + 'static + IDUpgradeTransport,
    S: UpgradeTransportServer<UpgradeTransport = StreamTransport<C>>,
{
    pub fn add_tls(
        self,
        acceptor: TlsAcceptor,
    ) -> UpgradeWebRTCServer<TlsUpgradeTransportServer<S>> {
        UpgradeWebRTCServer {
            server: TlsUpgradeTransportServer {
                server: self.server,
                acceptor,
            },
            api: self.api,
            config: self.config,
        }
    }

    pub fn add_tls_from_config(
        self,
        config: &TlsServerConfig,
    ) -> Result<UpgradeWebRTCServer<TlsUpgradeTransportServer<S>>, (TlsInitError, Self)> {
        let acceptor = match new_tls_acceptor(&config) {
            Ok(x) => x,
            Err(e) => return Err((e, self)),
        };
        Ok(self.add_tls(acceptor))
    }
}

pub async fn server_new_tcp(
    addr: impl ToSocketAddrs,
) -> std::io::Result<UpgradeWebRTCServer<TcpListener>> {
    Ok(UpgradeWebRTCServer::new(TcpListener::bind(addr).await?))
}

#[cfg(feature = "local_sockets")]
pub async fn server_new_local_socket<'a>(
    addr: impl interprocess::local_socket::ToLocalSocketName<'a>,
) -> std::io::Result<UpgradeWebRTCServer<interprocess::local_socket::tokio::LocalSocketListener>> {
    use interprocess::local_socket::tokio::LocalSocketListener;

    Ok(UpgradeWebRTCServer::new(LocalSocketListener::bind(addr)?))
}
