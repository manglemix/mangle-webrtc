#![feature(async_fn_in_trait)]

use serde::{Serialize, Deserialize};
use webrtc::{peer_connection::sdp::session_description::RTCSessionDescription, ice_transport::ice_candidate::RTCIceCandidateInit};

pub mod server;
pub mod transport;
pub mod client;


#[derive(Serialize, Deserialize)]
enum RTCMessage {
    SDPAnswer(RTCSessionDescription),
    ICE(RTCIceCandidateInit)
}


#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use crate::{server::server_new_tcp, client::client_new_tcp};

    #[tokio::test(flavor = "multi_thread")]
    async fn general_test01() {
        let mut server = server_new_tcp("127.0.0.1:8000").await.expect("Server should have initialized");
        let (server_sender, mut server_recv) = mpsc::channel(1);
        let handle = tokio::spawn(async move {
            server.run::<(), _, _>(|p| async { server_sender.send(p).await.unwrap(); }, |e| async move { panic!("Server Error: {e:?}") }).await;
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
        
        let mut client = expect!(client_new_tcp("127.0.0.1:8000").await, "Client should have initialized");
        expect!(client.upgrade().await, "Client should have upgraded");
        if server_recv.recv().await.is_none() {
            handle.abort();
            panic!("Server should have upgraded a client");
        }
        handle.abort();
    }
}