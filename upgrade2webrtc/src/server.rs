use std::{ops::ControlFlow, sync::Arc};

use futures::{stream::FuturesUnordered, Future, StreamExt};
use tokio::net::{TcpListener, ToSocketAddrs};
use webrtc::{
    api::{APIBuilder, API, media_engine::MediaEngine, interceptor_registry::register_default_interceptors},
    ice_transport::{ice_candidate::RTCIceCandidate, ice_server::RTCIceServer},
    peer_connection::{
        configuration::RTCConfiguration, sdp::session_description::RTCSessionDescription,
        RTCPeerConnection,
    }, interceptor::registry::Registry,
};

use crate::{transport::{IDUpgradeTransport, RecvError, UpgradeTransport, UpgradeTransportServer}, RTCMessage};

pub struct UpgradeWebRTCServer<S: UpgradeTransportServer> {
    server: S,
    api: API,
    config: RTCConfiguration,
}

#[derive(Debug)]
pub enum ServerError<AE, DE> {
    AcceptError(AE),
    BadMessage { error: DE, client_id: String },
    IOError(std::io::Error),
    WebRTCError(webrtc::Error),
}

impl<S: UpgradeTransportServer> UpgradeWebRTCServer<S> {
    pub fn new(server: S) -> Self {
        let mut m = MediaEngine::default();
        m.register_default_codecs().expect("Default codecs should have registered safely");

        let mut registry = Registry::new();

        // Use the default set of Interceptors
        registry = register_default_interceptors(registry, &mut m).expect("Default interceptors should have registered safely");

        Self {
            server,
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
            }
        }
    }

    pub async fn run<V, F1, F2>(
        &mut self,
        on_success: impl Fn(RTCPeerConnection) -> F1,
        on_err: impl Fn(
            ServerError<
                S::AcceptError,
                <S::UpgradeTransport as UpgradeTransport>::DeserializationError,
            >,
        ) -> F2,
    ) -> V
    where
        F1: Future<Output = ()>,
        F2: Future<Output = ControlFlow<V, ()>>,
    {
        let mut pending_peers = FuturesUnordered::new();

        loop {
            tokio::select! {
                result = self.server.accept() => {
                    let transport = match result {
                        Ok(x) => x,
                        Err(e) => if let ControlFlow::Break(x) = on_err(ServerError::AcceptError(e)).await {
                            break x
                        } else {
                            continue
                        }
                    };
                    pending_peers.push(async {
                        let mut transport = transport;

                        loop {
                            let offer: RTCSessionDescription = match transport.recv_obj().await {
                                Ok(x) => x,
                                Err(RecvError::DeserializeError(error)) => return on_err(ServerError::<S::AcceptError, _>::BadMessage {
                                        error,
                                        client_id: transport.get_id()
                                    }).await,
                                Err(RecvError::IOError(e)) => 
                                    return on_err(ServerError::IOError(e)).await
                            };
    
                            let peer = match self.api.new_peer_connection(self.config.clone()).await {
                                Ok(x) => x,
                                Err(e) => return on_err(ServerError::WebRTCError(e)).await
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
                                        Err(e) => returning!(on_err(ServerError::WebRTCError(e)).await)
                                    }
                                };
                            }
                            let _data_channel = webrtc_unwrap!(peer.create_data_channel("command", None).await);
    
                            let (ice_sender, mut ice_receiver) = tokio::sync::mpsc::channel(3);
                            let ice_sender = Arc::new(ice_sender);
    
                            peer.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
                                let ice_sender = ice_sender.clone();
                                Box::pin(async move { let _ = ice_sender.send(c).await; })
                            }));
    
                            webrtc_unwrap!(peer.set_remote_description(offer).await);
                            let answer = RTCMessage::SDPAnswer(webrtc_unwrap!(peer.create_answer(None).await));
                            if let Err(e) = transport.send_obj(&answer).await {
                                returning!(on_err(ServerError::IOError(e)).await)
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
                                        println!("s send {}", ice_to_send.is_some());
                                        if let Err(e) = transport.send_obj(&ice_to_send).await {
                                            returning!(on_err(ServerError::IOError(e)).await);  
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
                                            Err(RecvError::DeserializeError(error)) => returning!(on_err(ServerError::BadMessage { error, client_id: transport.get_id() }).await),
                                            Err(RecvError::IOError(e)) => returning!(on_err(ServerError::IOError(e)).await)
                                        };
                                        println!("s {}", received_ice.is_some());
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
    
                            on_success(peer).await;
                        }
                    });
                }
                result = pending_peers.next() => {
                    let Some(ControlFlow::Break(err)) = result else { continue };
                    return err
                }
            }
        }
    }
}

pub async fn server_new_tcp(addr: impl ToSocketAddrs) -> std::io::Result<UpgradeWebRTCServer<TcpListener>> {
    Ok(UpgradeWebRTCServer::new(TcpListener::bind(addr).await?))
}
