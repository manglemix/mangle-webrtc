use std::{sync::Arc, net::{SocketAddrV4, Ipv4Addr}};

use bytes::Bytes;
use clap::Subcommand;
use fxhash::FxHashMap;
use log::info;
use serde::Deserialize;
use tokio::{sync::mpsc, net::{UdpSocket, TcpStream}, io::{BufStream, AsyncWriteExt, AsyncReadExt}};
use upgrade2webrtc::{webrtc::data_channel::RTCDataChannel, tls::TlsServerConfig, server::{server_new_tcp, server_new_local_socket}};

use crate::{Channel, UDP_PACKET_SIZE};


#[derive(Deserialize)]
pub struct ServerConfig {
    /// The path to the root certificate.
    #[serde(default)]
    pub root_cert_chain_path: String,
    /// The path to the root private key.
    #[serde(default)]
    pub parent_key_path: String,
    /// Subject Alternative Names that will be used to create
    /// the server certificate.
    #[serde(default)]
    pub subject_alt_names: Vec<String>,
    #[serde(default)]
    pub create_if_missing: bool,
    channels: Vec<Channel>,
}

#[derive(Subcommand, Debug)]
pub(super) enum ServerStreamType {
    TCP { addr: String },
    LocalSocket { addr: String },
}

pub(super) async fn server_command(
    stream_type: ServerStreamType,
    config_path: String,
) -> anyhow::Result<()> {
    let config: ServerConfig = toml::from_str(std::fs::read_to_string(config_path)?.as_str())?;

    let mut allowed_channels = FxHashMap::default();
    for channel in config.channels {
        match &channel {
            Channel::UDP { port, remap, .. } => {
                allowed_channels.insert(remap.as_ref().unwrap_or(port).to_string(), channel)
            }
            Channel::TCP { port, remap, .. } => {
                allowed_channels.insert(remap.as_ref().unwrap_or(port).to_string(), channel)
            }
            Channel::LocalSocket { addr, remap, .. } => {
                allowed_channels.insert(remap.as_ref().unwrap_or(addr).clone(), channel)
            }
        };
    }
    let allowed_channels: &_ = Box::leak(Box::new(allowed_channels));

    let on_success = move |_peer,
                           mut channels_recv: mpsc::Receiver<Arc<RTCDataChannel>>,
                           transport_id,
                           _| async move {
        info!("New connection from {transport_id}");
        loop {
            let Some(data_channel) = channels_recv.recv().await else {
                break;
            };
            let Some(channel) = allowed_channels.get(data_channel.protocol()) else {
                let _ = data_channel.close().await;
                continue;
            };
            match channel {
                Channel::UDP { port, .. } => {
                    let socket = match UdpSocket::bind("127.0.0.1:0").await {
                        Ok(x) => x,
                        Err(e) => {
                            panic!("{e}");
                        }
                    };
                    if let Err(e) = socket
                        .connect(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port.get()))
                        .await
                    {
                        panic!("{e}");
                    }
                    let (bytes_sender, mut bytes_recv) = mpsc::channel(2);
                    let bytes_sender = Arc::new(bytes_sender);

                    data_channel.on_message(Box::new(move |msg| {
                        let bytes_sender = bytes_sender.clone();
                        Box::pin(async move {
                            let _ = bytes_sender.send(msg.data).await;
                        })
                    }));

                    let mut buf = [0u8; UDP_PACKET_SIZE];
                    tokio::select! {
                        result = socket.recv(&mut buf) => {
                            let n = match result {
                                Ok(x) => x,
                                Err(e) => {
                                    panic!("{e}");
                                }
                            };
                            let buf = Bytes::from(buf.split_at(n).0.to_vec());
                            match data_channel.send(&buf).await {
                                Ok(x) => debug_assert_eq!(x, n),
                                Err(e) => {
                                    panic!("{e}");
                                }
                            }
                        }
                        msg = bytes_recv.recv() => {
                            let Some(msg) = msg else {
                                panic!()
                            };
                            let mut sent_n = 0usize;
                            while sent_n < msg.len() {
                                sent_n += match socket.send(msg.split_at(sent_n).1).await {
                                    Ok(x) => x,
                                    Err(e) => {
                                        panic!("{e}");
                                    }
                                };
                            }
                        }
                    }
                }
                Channel::TCP { port, .. } => {
                    let socket = match TcpStream::connect(SocketAddrV4::new(
                        Ipv4Addr::LOCALHOST,
                        port.get(),
                    ))
                    .await
                    {
                        Ok(x) => x,
                        Err(e) => {
                            panic!("{e}");
                        }
                    };
                    let mut socket = BufStream::new(socket);
                    let (bytes_sender, mut bytes_recv) = mpsc::channel(2);
                    let bytes_sender = Arc::new(bytes_sender);

                    data_channel.on_message(Box::new(move |msg| {
                        info!("Received bytes");
                        let bytes_sender = bytes_sender.clone();
                        Box::pin(async move {
                            let _ = bytes_sender.send(msg.data).await;
                        })
                    }));

                    let mut buf = [0u8; 2048];
                    tokio::select! {
                        result = socket.read(&mut buf) => {
                            let n = match result {
                                Ok(x) => x,
                                Err(e) => {
                                    panic!("{e}");
                                }
                            };
                            let buf = Bytes::from(buf.split_at(n).0.to_vec());
                            match data_channel.send(&buf).await {
                                Ok(x) => debug_assert_eq!(x, n),
                                Err(e) => {
                                    panic!("{e}");
                                }
                            }
                        }
                        msg = bytes_recv.recv() => {
                            let Some(msg) = msg else {
                                panic!()
                            };
                            if let Err(e) = socket.write_all(&msg).await {
                                panic!("{e}");
                            }
                            if let Err(e) = socket.flush().await {
                                panic!("{e}");
                            }
                        }
                    }
                }
                Channel::LocalSocket { addr: _, .. } => {}
            }
        }
    };

    macro_rules! on_err {
        () => {
            |err, _| async move { panic!("LMao {err}") }
        };
    }
    let tls_config = TlsServerConfig {
        root_cert_chain_path: config.root_cert_chain_path,
        parent_key_path: config.parent_key_path,
        subject_alt_names: config.subject_alt_names,
        create_if_missing: config.create_if_missing,
    };

    match stream_type {
        ServerStreamType::TCP { addr } => {
            let mut server = server_new_tcp(addr).await?;
            if tls_config.root_cert_chain_path.is_empty() {
                server.run(on_success, on_err!()).await
            } else {
                let mut server = server.add_tls_from_config(&tls_config).map_err(|e| e.0)?;
                server.run(on_success, on_err!()).await
            }
        }
        ServerStreamType::LocalSocket { addr } => {
            let mut server = server_new_local_socket(addr).await?;
            if tls_config.root_cert_chain_path.is_empty() {
                server.run(on_success, on_err!()).await
            } else {
                let mut server = server.add_tls_from_config(&tls_config).map_err(|e| e.0)?;
                server.run(on_success, on_err!()).await
            }
        }
    }

    Ok(())
}
