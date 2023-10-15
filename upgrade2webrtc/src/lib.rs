use serde::{Deserialize, Serialize};
use webrtc::{
    data_channel::data_channel_init::RTCDataChannelInit,
    ice_transport::ice_candidate::RTCIceCandidateInit,
    peer_connection::sdp::session_description::RTCSessionDescription,
};

pub mod client;
pub mod server;
pub mod tls;
pub mod transport;

pub use webrtc;

pub const STUN_SERVERS: [&str; 19] = [
    "stun:stun.l.google.com:19302",
    "stun:stun1.l.google.com:19302",
    "stun:stun2.l.google.com:19302",
    "stun:stun3.l.google.com:19302",
    "stun:stun4.l.google.com:19302",
    "stun:stun01.sipphone.com",
    "stun:stun.ekiga.net",
    "stun:stun.fwdnet.net",
    "stun:stun.ideasip.com",
    "stun:stun.iptel.org",
    "stun:stun.rixtelecom.se",
    "stun:stun.schlund.de",
    "stun:stunserver.org",
    "stun:stun.softjoys.com",
    "stun:stun.voiparound.com",
    "stun:stun.voipbuster.com",
    "stun:stun.voipstunt.com",
    "stun:stun.voxgratia.org",
    "stun:stun.xten.com"
];

/// Configuration for a data channel that is equivalent to a TCP connection
pub const TCP_DATA_CHANNEL: RTCDataChannelInit = RTCDataChannelInit {
    ordered: Some(true),
    max_packet_life_time: None,
    max_retransmits: Some(u16::MAX),
    protocol: None,
    negotiated: None,
};

/// Configuration for a data channel that is equivalent to a UDP connection
pub const UDP_DATA_CHANNEL: RTCDataChannelInit = RTCDataChannelInit {
    ordered: Some(false),
    max_packet_life_time: None,
    max_retransmits: Some(0),
    protocol: None,
    negotiated: None,
};

/// Configuration for a data channel where messages are always received
/// but may not be in order
pub const RELIABLE_UNORDERED_DATA_CHANNEL: RTCDataChannelInit = RTCDataChannelInit {
    ordered: Some(false),
    max_packet_life_time: None,
    max_retransmits: Some(u16::MAX),
    protocol: None,
    negotiated: None,
};

#[derive(Serialize, Deserialize)]
enum RTCMessage {
    SDPAnswer(RTCSessionDescription),
    ICE(RTCIceCandidateInit),
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use crate::{client::client_new_tcp, server::server_new_tcp, TCP_DATA_CHANNEL};

    #[tokio::test(flavor = "multi_thread")]
    async fn local_tcp_test_01() {
        let mut server = server_new_tcp("127.0.0.1:8000")
            .await
            .expect("Server should have initialized");
        let (server_sender, mut server_recv) = mpsc::channel(1);
        let server_sender: &_ = Box::leak(Box::new(server_sender));
        let handle = tokio::spawn(async move {
            server
                .run::<_, _>(
                    move |p, _, _, _| async move {
                        server_sender.send(p).await.unwrap();
                    },
                    |e, _| async move { panic!("Server Error: {e:?}") },
                )
                .await;
        });
        macro_rules! expect {
            ($result: expr, $msg: expr) => {{
                let result = $result;
                if result.is_err() {
                    handle.abort();
                }
                result.expect($msg)
            }};
        }

        let mut client = expect!(
            client_new_tcp("127.0.0.1:8000").await,
            "Client should have initialized"
        );
        expect!(
            client.upgrade([("test", TCP_DATA_CHANNEL)]).await,
            "Client should have upgraded"
        );
        if server_recv.recv().await.is_none() {
            handle.abort();
            panic!("Server should have upgraded a client");
        }
        handle.abort();
    }
}
