use std::fmt::Debug;

use async_trait::async_trait;
use serde::{de::DeserializeOwned, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufStream};
#[cfg(feature = "local_sockets")]
use tokio_util::compat::{Compat, FuturesAsyncWriteCompatExt};

#[cfg(feature = "local_sockets")]
use interprocess::local_socket::tokio::{LocalSocketListener, LocalSocketStream};

#[async_trait]
pub trait UpgradeTransport: Send + Sync + 'static {
    type DeserializationError: std::error::Error + Send;

    /// # Cancel Safety
    /// This method is not cancel safe according to the `tokio` guidelines
    async fn send_obj(&mut self, obj: &(impl Serialize + Sync)) -> std::io::Result<()>;

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

#[async_trait]
pub trait UpgradeTransportServer: Send {
    type AcceptError: std::error::Error + Send;
    type UpgradeTransport: UpgradeTransport + IDUpgradeTransport;

    async fn accept(&mut self) -> Result<Self::UpgradeTransport, Self::AcceptError>;
}

#[async_trait]
impl UpgradeTransportServer for tokio::net::TcpListener {
    type AcceptError = std::io::Error;

    type UpgradeTransport = StreamTransport<BufStream<tokio::net::TcpStream>>;

    async fn accept(&mut self) -> Result<Self::UpgradeTransport, Self::AcceptError> {
        tokio::net::TcpListener::accept(self)
            .await
            .map(|x| BufStream::new(x.0).into())
    }
}

#[async_trait]
#[cfg(feature = "local_sockets")]
impl UpgradeTransportServer for LocalSocketListener {
    type AcceptError = std::io::Error;

    type UpgradeTransport = StreamTransport<BufStream<Compat<LocalSocketStream>>>;

    async fn accept(&mut self) -> Result<Self::UpgradeTransport, Self::AcceptError> {
        LocalSocketListener::accept(self)
            .await
            .map(|x| (BufStream::new(x.compat_write())).into())
    }
}

/// An `UpgradeTransport` that uses stream based communication such as TCP
/// (as opposed to a message based protocol like HTTP).
///
/// This relies on the stream being *reliable* but not necessarily ordered.
/// Unreliable protocols such as UDP must be wrapped in a struct that makes it reliable.
pub struct StreamTransport<T: AsyncWrite + AsyncRead + Send + Unpin + 'static> {
    pub(crate) stream: T,
    recv_buffer: Vec<u8>,
}

impl<T: AsyncWrite + AsyncRead + Send + Unpin + Debug + 'static> Debug for StreamTransport<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamTransport")
            .field("stream", &self.stream)
            .field("recv_buffer", &hex::encode(&self.recv_buffer))
            .finish()
    }
}

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

#[async_trait]
impl<T: AsyncWrite + AsyncRead + Send + Sync + Unpin + 'static> UpgradeTransport
    for StreamTransport<T>
{
    type DeserializationError = bitcode::Error;

    async fn send_obj(&mut self, obj: &(impl Serialize + Sync)) -> std::io::Result<()> {
        let msg = bitcode::serialize(obj).expect("The given object should be serializable");
        let size: u32 = msg
            .len()
            .try_into()
            .expect("Length of serialized bytes of the given object should be less than u32::MAX");
        self.stream.write_all(&size.to_le_bytes()).await?;
        self.stream.write_all(&msg).await?;
        self.stream.flush().await
    }

    async fn recv_obj<V: DeserializeOwned>(
        &mut self,
    ) -> Result<V, RecvError<Self::DeserializationError>> {
        if self.recv_buffer.len() >= 4 {
            let size: [u8; 4] = self.recv_buffer.split_at(4).0.try_into().unwrap();
            let size = u32::from_le_bytes(size) as usize + 4;
            if self.recv_buffer.len() >= size {
                let buf = self.recv_buffer.split_at(size).0.split_at(4).1;
                let result = bitcode::deserialize(buf).map_err(RecvError::DeserializeError);
                self.recv_buffer.drain(0..size);
                return result
            }
        }

        let mut buf = [0u8; 1024];
        loop {
            let n = self
                .stream
                .read(&mut buf)
                .await
                .map_err(RecvError::IOError)?;
            if n == 0 {
                // continue
                return Err(RecvError::IOError(std::io::Error::from(
                    std::io::ErrorKind::UnexpectedEof,
                )));
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
            let buf = self.recv_buffer.split_at(size).0.split_at(4).1;
            let result = bitcode::deserialize(buf).map_err(RecvError::DeserializeError);
            self.recv_buffer.drain(0..size);
            break result;
        }
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush().await
    }
}

impl<S: IDUpgradeTransport + AsyncWrite + AsyncRead + Send + Unpin + 'static> IDUpgradeTransport
    for StreamTransport<S>
{
    fn get_id(&self) -> String {
        self.stream.get_id()
    }
}

impl IDUpgradeTransport for tokio::net::TcpStream {
    fn get_id(&self) -> String {
        self.peer_addr()
            .map(|x| x.to_string())
            .unwrap_or_else(|e| format!("GET_ID_ERROR: {e}"))
    }
}

impl<S: IDUpgradeTransport> IDUpgradeTransport for tokio_rustls::TlsStream<S> {
    fn get_id(&self) -> String {
        self.get_ref().0.get_id()
    }
}

impl<S: IDUpgradeTransport> IDUpgradeTransport for tokio_rustls::client::TlsStream<S> {
    fn get_id(&self) -> String {
        self.get_ref().0.get_id()
    }
}

impl<S: IDUpgradeTransport> IDUpgradeTransport for tokio_rustls::server::TlsStream<S> {
    fn get_id(&self) -> String {
        self.get_ref().0.get_id()
    }
}

#[cfg(feature = "local_sockets")]
impl<S: IDUpgradeTransport> IDUpgradeTransport for Compat<S> {
    fn get_id(&self) -> String {
        self.get_ref().get_id()
    }
}

impl<S: IDUpgradeTransport + AsyncRead + AsyncWrite> IDUpgradeTransport for BufStream<S> {
    fn get_id(&self) -> String {
        self.get_ref().get_id()
    }
}

impl IDUpgradeTransport for tokio::io::DuplexStream {
    fn get_id(&self) -> String {
        format!("{:p}", self)
    }
}

#[cfg(feature = "local_sockets")]
impl IDUpgradeTransport for LocalSocketStream {
    fn get_id(&self) -> String {
        self.peer_pid()
            .map(|x| format!("PID({x})"))
            .unwrap_or_else(|e| format!("GET_ID_ERROR: {e}"))
    }
}
