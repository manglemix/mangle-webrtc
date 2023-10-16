use std::{
    collections::hash_map::Entry,
    future::Future,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    ops::{Deref, DerefMut},
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use bytes::Bytes;
use clap::Subcommand;
use fxhash::FxHashMap;
use log::{error, info};
use serde::Deserialize;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UdpSocket},
    sync::{broadcast, mpsc, oneshot},
    task::JoinSet,
};
use upgrade2webrtc::{
    client::{client_new_local_socket, client_new_tcp, PeerAndChannels, ServerName},
    webrtc::{
        data_channel::data_channel_init::RTCDataChannelInit, peer_connection::RTCPeerConnection,
    },
    TCP_DATA_CHANNEL,
};

use crate::{Channel, UDP_PACKET_SIZE};

#[derive(Deserialize)]
struct ClientConfig {
    #[serde(default)]
    use_tls: bool,
    root_cert_path: Option<String>,
    domain_name: Option<String>,
    channels: Vec<Channel>,
}

#[derive(Subcommand, Debug)]
pub(super) enum ClientStreamType {
    TCP { addr: String },
    LocalSocket { addr: String },
}

pub(super) async fn client_command(
    stream_type: ClientStreamType,
    config_path: String,
) -> anyhow::Result<()> {
    let config: ClientConfig = toml::from_str(std::fs::read_to_string(config_path)?.as_str())?;

    if config.channels.is_empty() {
        return Err(anyhow::anyhow!("No channels provided"));
    }
    let init_channels = [("init", TCP_DATA_CHANNEL)];

    let PeerAndChannels { peer, mut channels } = match stream_type {
        ClientStreamType::TCP { addr } => {
            let mut client = client_new_tcp(addr).await?;
            if config.use_tls {
                let domain = ServerName::try_from(
                    config
                        .domain_name
                        .as_ref()
                        .ok_or(anyhow::anyhow!("Missing domain name"))?
                        .as_str(),
                )?;
                let mut client = client
                    .add_tls_from_config(domain, config.root_cert_path)
                    .await?;
                client.upgrade(init_channels).await?
            } else {
                client.upgrade(init_channels).await?
            }
        }
        ClientStreamType::LocalSocket { addr } => {
            let mut client = client_new_local_socket(addr).await?;
            if config.use_tls {
                let domain = ServerName::try_from(
                    config
                        .domain_name
                        .as_ref()
                        .ok_or(anyhow::anyhow!("Missing domain name"))?
                        .as_str(),
                )?;
                let mut client = client
                    .add_tls_from_config(domain, config.root_cert_path)
                    .await?;
                client.upgrade(init_channels).await?
            } else {
                client.upgrade(init_channels).await?
            }
        }
    };
    let init_channel = channels.pop().unwrap();
    init_channel.1.close().await?;
    info!("Successfully connected to server");

    struct PeerWrapper(Option<RTCPeerConnection>);

    impl Deref for PeerWrapper {
        type Target = RTCPeerConnection;

        fn deref(&self) -> &Self::Target {
            self.0.as_ref().unwrap()
        }
    }

    impl DerefMut for PeerWrapper {
        fn deref_mut(&mut self) -> &mut Self::Target {
            self.0.as_mut().unwrap()
        }
    }

    impl Drop for PeerWrapper {
        fn drop(&mut self) {
            let peer = self.0.take().unwrap();
            tokio::spawn(async move {
                let _ = peer.close().await;
            });
        }
    }

    let peer = Arc::new(PeerWrapper(Some(peer)));
    let channel_counter = Arc::new(AtomicUsize::new(0));
    let mut tasks = JoinSet::<anyhow::Result<()>>::new();
    let (end_sender, _) = broadcast::channel::<()>(1);

    for channel in config.channels {
        let peer = peer.clone();
        let channel_counter = channel_counter.clone();
        let mut end_receiver = end_sender.subscribe();

        tasks.spawn(async move {
            match channel {
                Channel::UDP { port, quality, remap } => {
                    let quality = quality.expect(&format!("Missing quality setting for port {port}"));
                    let listener_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, remap.unwrap_or(port).get());
                    let listener = UdpSocket::bind(listener_addr).await?;
                    let mut buf = vec![0u8; UDP_PACKET_SIZE];
                    let mut existing_channel_senders: FxHashMap<SocketAddr, mpsc::Sender<Bytes>> = FxHashMap::default();

                    loop {
                        tokio::select! {
                            result = listener.recv_buf_from(&mut buf) => {
                                let (n, addr) = result?;
                                let buf = Bytes::from(buf.as_slice().split_at(n).0.to_vec());
                                
                                match existing_channel_senders.entry(addr) {
                                    Entry::Occupied(x) => {
                                        if x.get().send(buf).await.is_err() {
                                            x.remove();
                                        }
                                    }
                                    Entry::Vacant(x) => {
                                        let socket = UdpSocket::bind(listener_addr).await?;
                                        socket.connect(addr).await?;

                                        let (listener_bytes_sender, mut listener_bytes_receiver) = mpsc::channel(3);

                                        let channel_idx = channel_counter.fetch_add(1, Ordering::Relaxed);
                                        let mut config: RTCDataChannelInit = quality.into();
                                        config.protocol = Some(port.to_string());
                                        let data_channel = peer.create_data_channel(&channel_idx.to_string(), Some(config)).await?;
                                        listener_bytes_sender.try_send(buf).unwrap();
                                        
                                        let (webrtc_bytes_sender, mut webrtc_bytes_recv) = mpsc::channel(2);
                                        let bytes_sender = Arc::new(webrtc_bytes_sender);

                                        data_channel.on_message(Box::new(move |msg| {
                                            let bytes_sender = bytes_sender.clone();
                                            Box::pin(async move {
                                                let _ = bytes_sender.send(msg.data).await;
                                            })
                                        }));

                                        tokio::spawn(async move {
                                            loop {
                                                tokio::select! {
                                                    result = listener_bytes_receiver.recv() => {
                                                        let Some(bytes) = result else { break };
                                                        let mut sent_n = 0;
                                                        while sent_n < bytes.len() {
                                                            sent_n += data_channel.send(&bytes.slice(sent_n..)).await.unwrap();
                                                        }
                                                    }
                                                    result = webrtc_bytes_recv.recv() => {
                                                        let Some(bytes) = result else { break };
                                                        socket.send(&bytes).await.unwrap();
                                                    }
                                                }
                                            }
                                        });

                                        x.insert(listener_bytes_sender);
                                    }
                                }
                            }
                            _ = end_receiver.recv() => {
                                break Ok(())
                            }
                        }
                    }
                }
                Channel::TCP { port, quality, remap } => {
                    let quality = quality.expect(&format!("Missing quality setting for port {port}"));
                    let listener_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, remap.unwrap_or(port).get());
                    let listener = TcpListener::bind(listener_addr).await?;

                    loop {
                        tokio::select! {
                            result = listener.accept() => {
                                let (mut stream, _) = result?;
                                let channel_idx = channel_counter.fetch_add(1, Ordering::Relaxed);
                                info!("New TCP connection ID: {channel_idx}");
                                let mut config: RTCDataChannelInit = quality.into();
                                config.protocol = Some(port.to_string());
                                let data_channel = peer.create_data_channel(&channel_idx.to_string(), Some(config)).await?;

                                let (open_sender, open_receiver) = oneshot::channel();

                                data_channel.on_open(Box::new(move || {
                                    let _ = open_sender.send(());
                                    Box::pin(std::future::ready(()))
                                }));

                                if open_receiver.await.is_err() {
                                    error!("Failed to initialize data channel for ID: {channel_idx}");
                                    continue
                                }

                                let (webrtc_bytes_sender, mut webrtc_bytes_recv) = mpsc::channel(2);
                                let bytes_sender = Arc::new(webrtc_bytes_sender);

                                data_channel.on_message(Box::new(move |msg| {
                                    let bytes_sender = bytes_sender.clone();
                                    Box::pin(async move {
                                        let _ = bytes_sender.send(msg.data).await;
                                    })
                                }));
                                info!("Data channel for ID: {channel_idx} established");

                                tokio::spawn(async move {
                                    let mut buf = vec![0u8; 2048];
                                    loop {
                                        tokio::select! {
                                            result = stream.read(&mut buf) => {
                                                let n = result.unwrap();
                                                let bytes = Bytes::from(buf.split_at(n).0.to_vec());
                                                let mut sent_n = 0;
                                                info!("Sending {n} bytes");
                                                while sent_n < n {
                                                    sent_n += data_channel.send(&bytes.slice(n..)).await.unwrap();
                                                    if sent_n > 0 {
                                                        info!("Sent {sent_n} bytes so far");
                                                    }
                                                }
                                            }
                                            result = webrtc_bytes_recv.recv() => {
                                                let Some(bytes) = result else { break };
                                                stream.write_all(&bytes).await.unwrap();
                                            }
                                        }
                                    }
                                });
                            }
                            _ = end_receiver.recv() => {
                                break Ok(())
                            }
                        }
                    }
                }
                Channel::LocalSocket {addr:_,quality:_, remap:_ } => todo!(),
            }
        });
    }

    let mut ctrl_c_fut: Pin<Box<dyn Future<Output = std::io::Result<()>>>> =
        Box::pin(tokio::signal::ctrl_c());

    let result = loop {
        tokio::select! {
            result = ctrl_c_fut => {
                if let Err(e) = result {
                    error!("Failed to check for Ctrl-C: {e}");
                    ctrl_c_fut = Box::pin(std::future::pending::<std::io::Result<()>>());
                    continue
                }
                break Ok(())
            }
            result = tasks.join_next() => {
                let result = result.unwrap().map_err(anyhow::Error::from);

                let result = match result {
                    Ok(x) => x,
                    Err(e) => Err(e)
                };

                break result
            }
        }
    };

    if result.is_err() {
        return result;
    }
    info!("Received Ctrl-C");

    tasks.abort_all();
    let _ = peer.close().await;
    info!("WebRTC Peer closed");
    drop(end_sender);
    while tasks.join_next().await.is_some() {}
    Ok(())
}
