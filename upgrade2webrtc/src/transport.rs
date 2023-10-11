use std::fmt::Debug;

use serde::{de::DeserializeOwned, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub trait UpgradeTransport: Send + 'static {
    type DeserializationError: std::error::Error;

    /// # Cancel Safety
    /// This method is not cancel safe according to the `tokio` guidelines
    async fn send_obj(&mut self, obj: &impl Serialize) -> std::io::Result<()>;
    /// # Cancel Safety
    /// This method *is* cancel safe according to the `tokio` guidelines
    async fn recv_obj<T: DeserializeOwned>(
        &mut self,
    ) -> Result<T, RecvError<Self::DeserializationError>>;

    async fn flush(&mut self) -> std::io::Result<()>;
}

pub trait IDUpgradeTransport {
    fn get_id(&self) -> String;
}

pub trait UpgradeTransportServer {
    type AcceptError: std::error::Error;
    type UpgradeTransport: UpgradeTransport + IDUpgradeTransport;

    async fn accept(&mut self) -> Result<Self::UpgradeTransport, Self::AcceptError>;
}


impl UpgradeTransportServer for tokio::net::TcpListener {
    type AcceptError = std::io::Error;

    type UpgradeTransport = StreamTransport<tokio::io::BufStream<tokio::net::TcpStream>>;

    async fn accept(&mut self) -> Result<Self::UpgradeTransport, Self::AcceptError> {
        tokio::net::TcpListener::accept(self).await.map(|x| tokio::io::BufStream::new(x.0).into())
    }
}

/// An `UpgradeTransport` that uses stream based communication such as TCP
/// (as opposed to a message based protocol like HTTP).
///
/// This relies on the stream being *reliable* but not necessarily ordered.
/// Unreliable protocols such as UDP must be wrapped in a struct that makes it reliable.
pub struct StreamTransport<T: AsyncWrite + AsyncRead + Send + Unpin + 'static> {
    stream: T,
    recv_buffer: Vec<u8>,
}


impl<T: AsyncWrite + AsyncRead + Send + Unpin + Debug + 'static> Debug for StreamTransport<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamTransport").field("stream", &self.stream).field("recv_buffer", &hex::encode(&self.recv_buffer)).finish()
    }
}


// impl<T: AsyncWrite + AsyncRead + Send + Unpin + 'static> Drop for StreamTransport<T> {
//     fn drop(&mut self) {
//         unreachable!()
//     }
// }


impl<T: AsyncWrite + AsyncRead + Send + Unpin + 'static> From<T> for StreamTransport<T> {
    fn from(stream: T) -> Self {
        Self {
            stream,
            recv_buffer: vec![],
        }
    }
}

#[derive(Debug)]
pub enum RecvError<E> {
    DeserializeError(E),
    IOError(std::io::Error),
}


impl<T: AsyncWrite + AsyncRead + Send + Unpin + 'static> UpgradeTransport for StreamTransport<T> {
    // type DeserializationError = serde_json::Error;
    type DeserializationError = bitcode::Error;

    async fn send_obj(&mut self, obj: &impl Serialize) -> std::io::Result<()> {
        // let msg = serde_json::to_vec_pretty(obj).expect("The given object should be serializable");
        // println!("{}", String::from_utf8(msg.clone()).unwrap());
        let msg = bitcode::serialize(obj).expect("The given object should be serializable");
        let size: u32 = msg
            .len()
            .try_into()
            .expect("Length of serialized bytes of the given object should be less than u32::MAX");
        println!("{size}");
        self.stream.write_all(&size.to_le_bytes()).await?;
        self.stream.write_all(&msg).await?;
        self.stream.flush().await
    }

    async fn recv_obj<V: DeserializeOwned>(
        &mut self,
    ) -> Result<V, RecvError<Self::DeserializationError>> {
        let mut buf = [0u8; 1024];
        loop {
            // println!("{}", self.recv_buffer.len());
            let n = self
                .stream
                .read(&mut buf)
                .await
                .map_err(RecvError::IOError)?;
            if n == 0 {
                // continue
                return Err(RecvError::IOError(std::io::Error::from(std::io::ErrorKind::UnexpectedEof)))
            }
            self.recv_buffer.extend_from_slice(buf.split_at(n).0);
            if self.recv_buffer.len() < 4 {
                continue;
            }
            let size: [u8; 4] = self.recv_buffer.split_at(4).0.try_into().unwrap();
            let size = u32::from_le_bytes(size) as usize + 4;
            if self.recv_buffer.len() < size {
                continue;
            }
            // println!("r {:?}", self.recv_buffer.split_at(size).0.split_at(4).1.split_at(50).0);
            let buf = self.recv_buffer.split_at(size).0.split_at(4).1;
            // println!("{}", String::from_utf8(buf.to_vec()).unwrap());
            // let result = serde_json::from_slice(buf).map_err(RecvError::DeserializeError);
            let result = bitcode::deserialize(buf).map_err(RecvError::DeserializeError);
            #[cfg(debug_assertions)]
            let len = self.recv_buffer.len();
            self.recv_buffer.drain(0..size);
            debug_assert_eq!(len - size, self.recv_buffer.len());
            break result;
        }
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush().await
    }
}

impl IDUpgradeTransport for StreamTransport<tokio::net::TcpStream> {
    fn get_id(&self) -> String {
        self.stream
            .peer_addr()
            .map(|x| x.to_string())
            .unwrap_or_else(|e| format!("GET_ID_ERROR: {e}"))
    }
}

impl IDUpgradeTransport for StreamTransport<tokio::io::BufStream<tokio::net::TcpStream>> {
    fn get_id(&self) -> String {
        self.stream
            .get_ref()
            .peer_addr()
            .map(|x| x.to_string())
            .unwrap_or_else(|e| format!("GET_ID_ERROR: {e}"))
    }
}

impl IDUpgradeTransport for StreamTransport<tokio::io::DuplexStream> {
    fn get_id(&self) -> String {
        format!("{:p}", self)
    }
}
