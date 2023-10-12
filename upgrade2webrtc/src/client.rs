use std::{
    fmt::{Debug, Display},
    path::Path,
};

use tokio::{
    io::{AsyncRead, AsyncWrite, BufStream},
    net::{TcpStream, ToSocketAddrs},
};
pub use tokio_rustls::rustls::ServerName;
use tokio_rustls::{client::TlsStream, TlsConnector};
use webrtc::{
    api::{
        interceptor_registry::register_default_interceptors, media_engine::MediaEngine, APIBuilder,
        API,
    },
    ice_transport::{ice_candidate::RTCIceCandidate, ice_server::RTCIceServer},
    interceptor::registry::Registry,
    peer_connection::{configuration::RTCConfiguration, RTCPeerConnection},
};

use crate::{
    tls::{new_tls_connector, TlsInitError},
    transport::{IDUpgradeTransport, RecvError, StreamTransport, UpgradeTransport},
    RTCMessage,
};

pub struct UpgradeWebRTCClient<C: UpgradeTransport> {
    client: C,
    api: API,
    config: RTCConfiguration,
}

impl<C: Debug + UpgradeTransport> Debug for UpgradeWebRTCClient<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpgradeWebRTCClient")
            .field("client", &self.client)
            .finish()
    }
}

#[derive(Debug)]
pub enum ClientError<DE> {
    WebRTCError(webrtc::Error),
    IOError(std::io::Error),
    DeserializeError(DE),
    UnexpectedMessage,
}

impl<DE> From<webrtc::Error> for ClientError<DE> {
    fn from(value: webrtc::Error) -> Self {
        ClientError::WebRTCError(value)
    }
}

impl<DE> From<std::io::Error> for ClientError<DE> {
    fn from(value: std::io::Error) -> Self {
        ClientError::IOError(value)
    }
}

impl<DE> From<RecvError<DE>> for ClientError<DE> {
    fn from(value: RecvError<DE>) -> Self {
        match value {
            RecvError::DeserializeError(e) => Self::DeserializeError(e),
            RecvError::IOError(e) => Self::IOError(e),
        }
    }
}

impl<DE: Display> Display for ClientError<DE> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::WebRTCError(e) => write!(f, "{e}"),
            ClientError::IOError(e) => write!(f, "{e}"),
            ClientError::DeserializeError(e) => write!(f, "{e}"),
            ClientError::UnexpectedMessage => write!(f, "Unexpected WebRTC SDP type"),
        }
    }
}

impl<DE: Display + Debug> std::error::Error for ClientError<DE> {}

impl<C: UpgradeTransport> UpgradeWebRTCClient<C> {
    pub fn new(client: C) -> Self {
        let mut m = MediaEngine::default();
        m.register_default_codecs()
            .expect("Default codecs should have registered safely");

        let mut registry = Registry::new();

        // Use the default set of Interceptors
        registry = register_default_interceptors(registry, &mut m)
            .expect("Default interceptors should have registered safely");

        Self {
            client,
            api: APIBuilder::new()
                .with_media_engine(m)
                .with_interceptor_registry(registry)
                .build(),
            config: RTCConfiguration {
                ice_servers: vec![RTCIceServer {
                    urls: vec!["stun:stun.l.google.com:19302".to_owned()],
                    ..Default::default()
                }],
                ..Default::default()
            },
        }
    }

    pub async fn upgrade(
        &mut self,
    ) -> Result<RTCPeerConnection, ClientError<C::DeserializationError>> {
        let peer = self.api.new_peer_connection(self.config.clone()).await?;
        let _data_channel = peer.create_data_channel("command", None).await?;
        let offer = peer.create_offer(None).await?;
        self.client.send_obj(&offer).await?;
        peer.set_local_description(offer).await?;
        let mut ices = vec![];
        let answer = loop {
            let msg: RTCMessage = self.client.recv_obj().await?;
            match msg {
                RTCMessage::SDPAnswer(x) => break x,
                RTCMessage::ICE(x) => ices.push(x),
            }
        };
        peer.set_remote_description(answer).await?;
        for ice in ices {
            peer.add_ice_candidate(ice).await?;
        }

        let (ice_sender, mut ice_receiver) = tokio::sync::mpsc::channel(3);

        peer.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
            let ice_sender = ice_sender.clone();
            Box::pin(async move {
                let _ = ice_sender.send(c).await;
            })
        }));

        let mut done_sending_ice = false;
        let mut done_receiving_ice = false;

        loop {
            tokio::select! {
                ice_to_send = ice_receiver.recv() => {
                    let ice_to_send = ice_to_send.unwrap();
                    self.client.send_obj(&ice_to_send).await?;
                    if ice_to_send.is_none() {
                        done_sending_ice = true;
                        if done_receiving_ice {
                            break
                        }
                    };
                }
                received_msg = self.client.recv_obj::<Option<RTCMessage>>() => {
                    let received_msg = received_msg?;
                    let received_ice = match received_msg {
                        Some(RTCMessage::ICE(x)) => Some(x),
                        None => None,
                        _ => return Err(ClientError::UnexpectedMessage)
                    };
                    let Some(received_ice) = received_ice else {
                        done_receiving_ice = true;
                        if done_sending_ice {
                            break
                        }
                        continue
                    };
                    peer.add_ice_candidate(received_ice).await?;
                }
            }
        }

        // if let Err(e) = self.client.flush().await {
        //     let _ = peer.close().await;
        //     return Err(ClientError::IOError(e))
        // }

        Ok(peer)
    }
}

// pub struct TlsUpgradeTransport<S: AsyncWrite + AsyncRead + Send + Unpin + 'static + IDUpgradeTransport> {
//     stream: TlsSt<S>,
//     acceptor: TlsConnector
// }

impl<C> UpgradeWebRTCClient<StreamTransport<C>>
where
    C: AsyncWrite + AsyncRead + Send + Sync + Unpin + 'static + IDUpgradeTransport,
{
    pub async fn add_tls(
        self,
        domain: ServerName,
        connector: &TlsConnector,
    ) -> std::io::Result<UpgradeWebRTCClient<StreamTransport<TlsStream<C>>>> {
        let stream = connector.connect(domain, self.client.stream).await?;
        Ok(UpgradeWebRTCClient {
            client: StreamTransport::from(stream),
            api: self.api,
            config: self.config,
        })
    }

    pub async fn add_tls_from_config(
        self,
        domain: ServerName,
        root_cert_path: Option<impl AsRef<Path>>,
    ) -> Result<UpgradeWebRTCClient<StreamTransport<TlsStream<C>>>, TlsInitError> {
        let connector = new_tls_connector(root_cert_path)?;
        self.add_tls(domain, &connector).await.map_err(Into::into)
    }
}

pub async fn client_new_tcp(
    addr: impl ToSocketAddrs,
) -> std::io::Result<UpgradeWebRTCClient<StreamTransport<BufStream<TcpStream>>>> {
    Ok(UpgradeWebRTCClient::new(
        BufStream::new(TcpStream::connect(addr).await?).into(),
    ))
}

#[cfg(feature = "local_sockets")]
pub async fn client_new_local_socket<'a>(
    addr: impl interprocess::local_socket::ToLocalSocketName<'a>,
) -> std::io::Result<
    UpgradeWebRTCClient<
        StreamTransport<
            BufStream<
                tokio_util::compat::Compat<interprocess::local_socket::tokio::LocalSocketStream>,
            >,
        >,
    >,
> {
    use interprocess::local_socket::tokio::LocalSocketStream;
    use tokio_util::compat::FuturesAsyncWriteCompatExt;

    Ok(UpgradeWebRTCClient::new(
        BufStream::new(LocalSocketStream::connect(addr).await?.compat_write()).into(),
    ))
}
