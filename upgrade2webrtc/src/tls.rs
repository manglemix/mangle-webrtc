use std::{fmt::Display, fs::File, io::BufReader, path::Path, sync::Arc};

use rcgen::{generate_simple_self_signed, CertificateParams, KeyPair};
use serde::{Deserialize, Serialize};
use tokio_rustls::{
    rustls::{self, ClientConfig, RootCertStore, ServerConfig},
    TlsAcceptor, TlsConnector,
};

#[derive(Serialize, Deserialize)]
pub struct TlsServerConfig {
    /// The path to the root certificate.
    pub root_cert_chain_path: String,
    /// The path to the root private key.
    pub parent_key_path: String,
    /// Subject Alternative Names that will be used to create
    /// the server certificate.
    pub subject_alt_names: Vec<String>,
    pub create_if_missing: bool,
}

#[derive(Debug)]
pub enum TlsInitError {
    IOError(std::io::Error),
    RCGenError(rcgen::RcgenError),
    RustlsError(rustls::Error),
}

impl Display for TlsInitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TlsInitError::IOError(e) => write!(f, "{}", e),
            TlsInitError::RCGenError(e) => write!(f, "{}", e),
            TlsInitError::RustlsError(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for TlsInitError {}

impl From<std::io::Error> for TlsInitError {
    fn from(value: std::io::Error) -> Self {
        Self::IOError(value)
    }
}

impl From<rcgen::RcgenError> for TlsInitError {
    fn from(value: rcgen::RcgenError) -> Self {
        Self::RCGenError(value)
    }
}

impl From<rustls::Error> for TlsInitError {
    fn from(value: rustls::Error) -> Self {
        Self::RustlsError(value)
    }
}

pub(crate) fn new_tls_acceptor(config: &TlsServerConfig) -> Result<TlsAcceptor, TlsInitError> {
    let parent_certs_path: &Path = config.root_cert_chain_path.as_ref();
    let parent_key_path: &Path = config.parent_key_path.as_ref();
    let mut server_certs = vec![];
    let server_key;
    let parent_cert;

    // let subject_alt_names = config
    //     .subject_alt_names
    //     .iter()
    //     .cloned()
    //     .map(|s| match s.parse() {
    //         Ok(ip) => SanType::IpAddress(ip),
    //         Err(_) => SanType::DnsName(s),
    //     })
    //     .collect::<Vec<_>>();

    if parent_certs_path.try_exists()? {
        let mut parent_key_file = BufReader::new(File::open(parent_key_path)?);
        let keys = rustls_pemfile::pkcs8_private_keys(&mut parent_key_file)?;
        let [parent_key] = keys.as_slice() else {
            return Err(TlsInitError::IOError(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "There must be exactly one private key in the parent key file",
            )));
        };
        let parent_key = KeyPair::from_der(parent_key)?;
        let mut parent_certs_file = BufReader::new(File::open(parent_certs_path)?);
        let certs = rustls_pemfile::certs(&mut parent_certs_file)?;
        let Some(parent_cert_der) = certs.last() else {
            return Err(TlsInitError::IOError(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "There must be at least one certificate in the root cert chain file",
            )));
        };
        parent_cert = rcgen::Certificate::from_params(CertificateParams::from_ca_cert_der(
            parent_cert_der,
            parent_key,
        )?)?;
    } else if config.create_if_missing {
        parent_cert = generate_simple_self_signed([])?;
        std::fs::write(parent_certs_path, parent_cert.serialize_pem()?)?;
        std::fs::write(parent_key_path, parent_cert.get_key_pair().serialize_pem())?;
    } else {
        return Err(TlsInitError::IOError(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Certificate file does not exist",
        )));
    }

    let rcgen_server_cert = generate_simple_self_signed(config.subject_alt_names.clone())?;
    server_certs.push(rustls::Certificate(
        rcgen_server_cert.serialize_der_with_signer(&parent_cert)?,
    ));
    server_certs.push(rustls::Certificate(parent_cert.serialize_der()?));
    server_key = rustls::PrivateKey(rcgen_server_cert.serialize_private_key_der());

    let config = ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(server_certs, server_key)?;

    Ok(TlsAcceptor::from(Arc::new(config)))
}

pub(crate) fn new_tls_connector(
    root_cert_path: Option<impl AsRef<Path>>,
) -> Result<TlsConnector, TlsInitError> {
    let mut root_store = RootCertStore::empty();

    if let Some(root_cert_path) = root_cert_path {
        let mut root_cert_file = BufReader::new(File::open(root_cert_path)?);
        let certs = rustls_pemfile::certs(&mut root_cert_file)?;
        let [root_cert] = certs.as_slice() else {
            return Err(TlsInitError::IOError(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "There must be exactly one certificate in the root cert file",
            )));
        };
        root_store.add(&rustls::Certificate(root_cert.clone()))?;
    }

    let config = ClientConfig::builder()
        .with_safe_defaults()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    Ok(TlsConnector::from(Arc::new(config)))
}
