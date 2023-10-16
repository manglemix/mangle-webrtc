use std::{
    net::{Ipv4Addr, SocketAddrV4, SocketAddr},
    num::NonZeroU16,
    sync::{Arc, atomic::AtomicUsize},
    time::SystemTime, ops::{DerefMut, Deref}, collections::hash_map::Entry,
    sync::atomic::Ordering, future::Future, pin::Pin
};

use anyhow::anyhow;
use bytes::Bytes;
use clap::{Parser, Subcommand};
use fxhash::FxHashMap;
use log::{info, error};
use serde::Deserialize;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UdpSocket, TcpListener},
    sync::{mpsc, broadcast, oneshot}, task::JoinSet,
};
use upgrade2webrtc::{
    client::{client_new_local_socket, client_new_tcp, PeerAndChannels, ServerName},
    webrtc::{data_channel::data_channel_init::RTCDataChannelInit, peer_connection::RTCPeerConnection},
    RELIABLE_UNORDERED_DATA_CHANNEL, TCP_DATA_CHANNEL, UDP_DATA_CHANNEL,
};
#[cfg(feature = "server")]
use upgrade2webrtc::{server::{server_new_local_socket, server_new_tcp}, tls::TlsServerConfig, webrtc::data_channel::RTCDataChannel};
#[cfg(feature = "server")]
use tokio::{
    io::BufStream,
    net::TcpStream,
};

const UDP_PACKET_SIZE: usize = 512;

#[derive(Deserialize, Clone, Copy)]
enum Quality {
    UDP,
    TCP,
    ReliableUnordered,
}

impl Into<RTCDataChannelInit> for Quality {
    fn into(self) -> RTCDataChannelInit {
        match self {
            Quality::UDP => UDP_DATA_CHANNEL,
            Quality::TCP => TCP_DATA_CHANNEL,
            Quality::ReliableUnordered => RELIABLE_UNORDERED_DATA_CHANNEL,
        }
    }
}

#[derive(Deserialize)]
enum Channel {
    UDP { port: NonZeroU16, remap: Option<NonZeroU16>, quality: Option<Quality> },
    TCP { port: NonZeroU16, remap: Option<NonZeroU16>, quality: Option<Quality> },
    LocalSocket { addr: String, remap: Option<String>, quality: Option<Quality> },
}

// impl Into<(String, RTCDataChannelInit)> for Channel {
//     fn into(self) -> (String, RTCDataChannelInit) {
//         let mut out: (String, RTCDataChannelInit) = match self {
//             Channel::UDP { port, quality } => (port.to_string(), quality.expect(&format!("Missing quality setting for port {port}")).into()),
//             Channel::TCP { port, quality } => (port.to_string(), quality.expect(&format!("Missing quality setting for port {port}")).into()),
//             Channel::LocalSocket { addr, quality } => {
//                 // Address must not be a port
//                 assert!(addr.parse::<u16>().is_err());
//                 (addr.clone(), quality.expect(&format!("Missing quality setting for local socket {addr}")).into())
//             }
//         };
//         out.1.protocol = Some(out.0.clone());
//         out
//     }
// }

#[cfg(feature = "server")]
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

#[derive(Deserialize)]
struct ClientConfig {
    #[serde(default)]
    use_tls: bool,
    root_cert_path: Option<String>,
    domain_name: Option<String>,
    channels: Vec<Channel>,
}

#[derive(Subcommand, Debug)]
enum ClientStreamType {
    TCP { addr: String },
    LocalSocket { addr: String },
}

#[derive(Subcommand, Debug)]
enum ServerStreamType {
    TCP { addr: String },
    LocalSocket { addr: String },
}

/// Doc comment
#[derive(Subcommand, Debug)]
enum Command {
    /// Doc comment
    Client {
        #[command(subcommand)]
        stream_type: ClientStreamType,
        config_path: String,
    },

    /// Doc comment
    #[cfg(feature = "server")]
    Server {
        #[command(subcommand)]
        stream_type: ServerStreamType,
        config_path: String,
    },
}

/// Simple program to greet a person
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let Args { command } = Args::parse();

    fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "[{} {} {}] {}",
                humantime::format_rfc3339_seconds(SystemTime::now()),
                record.level(),
                record.target(),
                message
            ))
        })
        .level(log::LevelFilter::Info)
        .chain(std::io::stdout())
        // .chain(fern::log_file("output.log")?)
        .apply()?;

    match command {
        Command::Client {
            stream_type,
            config_path,
        } => {
            let config: ClientConfig =
                toml::from_str(std::fs::read_to_string(config_path)?.as_str())?;

            if config.channels.is_empty() {
                return Err(anyhow::anyhow!("No channels provided"))
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
                                .ok_or(anyhow!("Missing domain name"))?
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
                                .ok_or(anyhow!("Missing domain name"))?
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
            
            let mut ctrl_c_fut: Pin<Box<dyn Future<Output=std::io::Result<()>>>> = Box::pin(tokio::signal::ctrl_c());

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
                return result
            }
            info!("Received Ctrl-C");

            tasks.abort_all();
            let _ = peer.close().await;
            info!("WebRTC Peer closed");
            drop(end_sender);
            while tasks.join_next().await.is_some() {}
            Ok(())
        }

        #[cfg(feature = "server")]
        Command::Server {
            stream_type,
            config_path,
        } => {
            let config: ServerConfig =
                toml::from_str(std::fs::read_to_string(config_path)?.as_str())?;

            let mut channels = FxHashMap::default();
            for channel in config.channels {
                match &channel {
                    Channel::UDP { port, remap, .. } => channels.insert(remap.as_ref().unwrap_or(port).to_string(), channel),
                    Channel::TCP { port, remap, .. } => channels.insert(remap.as_ref().unwrap_or(port).to_string(), channel),
                    Channel::LocalSocket { addr, remap, .. } => channels.insert(remap.as_ref().unwrap_or(addr).clone(), channel),
                };
            }
            let channels: &_ = Box::leak(Box::new(channels));

            let on_success = move |_peer,
                                   mut channels_recv: mpsc::Receiver<Arc<RTCDataChannel>>,
                                   transport_id,
                                   _| async move {
                info!("New connection from {transport_id}");
                loop {
                    let Some(data_channel) = channels_recv.recv().await else {
                        break;
                    };
                    let Some(channel) = channels.get(data_channel.protocol()) else {
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
                        let mut server =
                            server.add_tls_from_config(&tls_config).map_err(|e| e.0)?;
                        server.run(on_success, on_err!()).await
                    }
                }
                ServerStreamType::LocalSocket { addr } => {
                    let mut server = server_new_local_socket(addr).await?;
                    if tls_config.root_cert_chain_path.is_empty() {
                        server.run(on_success, on_err!()).await
                    } else {
                        let mut server =
                            server.add_tls_from_config(&tls_config).map_err(|e| e.0)?;
                        server.run(on_success, on_err!()).await
                    }
                }
            }

            Ok(())
        }
    }

}
