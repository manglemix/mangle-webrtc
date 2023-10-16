use std::{
    num::NonZeroU16,
    time::SystemTime,
};

use clap::{Parser, Subcommand};
use client::{client_command, ClientStreamType};
use serde::Deserialize;
use server::{server_command, ServerStreamType};
use upgrade2webrtc::{
    webrtc::data_channel::data_channel_init::RTCDataChannelInit, RELIABLE_UNORDERED_DATA_CHANNEL,
    TCP_DATA_CHANNEL, UDP_DATA_CHANNEL,
};

mod client;
#[cfg(feature = "server")]
mod server;

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
    UDP {
        port: NonZeroU16,
        remap: Option<NonZeroU16>,
        quality: Option<Quality>,
    },
    TCP {
        port: NonZeroU16,
        remap: Option<NonZeroU16>,
        quality: Option<Quality>,
    },
    LocalSocket {
        addr: String,
        remap: Option<String>,
        quality: Option<Quality>,
    },
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
        } => client_command(stream_type, config_path).await,

        #[cfg(feature = "server")]
        Command::Server {
            stream_type,
            config_path,
        } => server_command(stream_type, config_path).await,
    }
}
