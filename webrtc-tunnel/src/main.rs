use std::ops::ControlFlow;

use clap::{Parser, Subcommand};
use serde::Deserialize;
use upgrade2webrtc::{
    client::{client_new_local_socket, client_new_tcp, ServerName},
    server::{server_new_local_socket, server_new_tcp},
    tls::TlsServerConfig,
};

#[derive(Deserialize, Default)]
struct ClientConfig {
    #[serde(default)]
    use_tls: bool,
    root_cert_path: Option<String>,
}

#[derive(Subcommand, Debug)]
enum ClientStreamType {
    TCP { addr: String },
    LocalSocket { domain: String, addr: String },
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
        config_path: Option<String>,
    },

    /// Doc comment
    #[cfg(feature = "server")]
    Server {
        #[command(subcommand)]
        stream_type: ServerStreamType,
        config_path: Option<String>,
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

    match command {
        Command::Client {
            stream_type,
            config_path,
        } => {
            let config: ClientConfig = if let Some(config_path) = config_path {
                toml::from_str(std::fs::read_to_string(config_path)?.as_str())?
            } else {
                Default::default()
            };

            match stream_type {
                ClientStreamType::TCP { addr } => {
                    let domain = ServerName::try_from(
                        addr.split(":")
                            .next()
                            .ok_or(anyhow::anyhow!("Invalid socket address"))?,
                    )?;
                    let mut client = client_new_tcp(addr).await?;
                    let peer = if config.use_tls {
                        let mut client = client
                            .add_tls_from_config(domain, config.root_cert_path)
                            .await?;
                        client.upgrade().await?
                    } else {
                        client.upgrade().await?
                    };
                    println!("Success")
                }
                ClientStreamType::LocalSocket { domain, addr } => {
                    let domain = ServerName::try_from(domain.as_str())?;
                    let mut client = client_new_local_socket(addr).await?;
                    let peer = if config.use_tls {
                        let mut client = client
                            .add_tls_from_config(domain, config.root_cert_path)
                            .await?;
                        client.upgrade().await?
                    } else {
                        client.upgrade().await?
                    };
                    println!("Success")
                }
            }
        }
        #[cfg(feature = "server")]
        Command::Server {
            stream_type,
            config_path,
        } => {
            let config: Option<TlsServerConfig> = if let Some(config_path) = config_path {
                Some(toml::from_str(
                    std::fs::read_to_string(config_path)?.as_str(),
                )?)
            } else {
                None
            };

            match stream_type {
                ServerStreamType::TCP { addr } => {
                    let mut server = server_new_tcp(addr).await?;
                    if let Some(config) = config {
                        let mut server = server.add_tls_from_config(&config).map_err(|e| e.0)?;
                        server
                            .run(|peer, _| async { println!("Success") }, |e, _| async {})
                            .await
                    } else {
                        server
                            .run(|peer, _| async { println!("Success") }, |e, _| async {})
                            .await
                    };
                }
                ServerStreamType::LocalSocket { addr } => {
                    let mut server = server_new_local_socket(addr).await?;
                    if let Some(config) = config {
                        let mut server = server.add_tls_from_config(&config).map_err(|e| e.0)?;
                        server
                            .run(|peer, _| async { println!("Success") }, |e, _| async {})
                            .await
                    } else {
                        server
                            .run(|peer, _| async { println!("Success") }, |e, _| async {})
                            .await
                    };
                }
            }
        }
    }

    Ok(())
}
