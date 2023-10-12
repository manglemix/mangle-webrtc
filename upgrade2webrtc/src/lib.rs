#![feature(ptr_from_ref)]

use serde::{Deserialize, Serialize};
use webrtc::{
    ice_transport::ice_candidate::RTCIceCandidateInit,
    peer_connection::sdp::session_description::RTCSessionDescription,
};

pub mod client;
pub mod server;
pub mod tls;
pub mod transport;

#[derive(Serialize, Deserialize)]
enum RTCMessage {
    SDPAnswer(RTCSessionDescription),
    ICE(RTCIceCandidateInit),
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use crate::{client::client_new_tcp, server::server_new_tcp};

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
                    move |p, _| async move {
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
        expect!(client.upgrade().await, "Client should have upgraded");
        if server_recv.recv().await.is_none() {
            handle.abort();
            panic!("Server should have upgraded a client");
        }
        handle.abort();
    }
}
