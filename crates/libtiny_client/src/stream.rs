use lazy_static::lazy_static;
use std::{
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
};

#[cfg(feature = "tls-native")]
use tokio_native_tls::TlsStream;
#[cfg(feature = "tls-rustls")]
use tokio_rustls::client::TlsStream;

#[cfg(feature = "tls-native")]
lazy_static! {
    static ref TLS_CONNECTOR: tokio_native_tls::TlsConnector = tls_connector(None);
}

#[cfg(feature = "tls-native")]
fn tls_connector(pem: Option<&Vec<u8>>) -> tokio_native_tls::TlsConnector {
    use native_tls::Identity;

    let mut builder = native_tls::TlsConnector::builder();
    if let Some(pem) = pem {
        let identity = Identity::from_pkcs8(pem, pem).expect("X509 Cert and private key");
        builder.identity(identity);
    }
    tokio_native_tls::TlsConnector::from(builder.build().unwrap())
}

#[cfg(feature = "tls-rustls")]
lazy_static! {
    static ref TLS_CONNECTOR: tokio_rustls::TlsConnector = tls_connector(None);
}

#[cfg(feature = "tls-rustls")]
fn tls_connector(sasl: Option<&Vec<u8>>) -> tokio_rustls::TlsConnector {
    use std::io::{Cursor, Seek, SeekFrom};
    use tokio_rustls::rustls::{Certificate, ClientConfig, PrivateKey, RootCertStore};

    let mut roots = RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().expect("could not load platform certs") {
        roots.add(&Certificate(cert.0)).unwrap();
    }

    let builder = ClientConfig::builder()
        .with_safe_defaults()
        .with_root_certificates(roots);

    let config = if let Some(pem) = sasl {
        let mut buf = Cursor::new(pem);
        // extract certificate
        let cert = rustls_pemfile::certs(&mut buf)
            .expect("Could not parse PKCS8 PEM")
            .pop()
            .expect("Cert PEM must have at least one cert");

        // extract private key
        buf.seek(SeekFrom::Start(0)).unwrap();
        let key = rustls_pemfile::pkcs8_private_keys(&mut buf)
            .expect("Could not parse PKCS8 PEM")
            .pop()
            .expect("Cert PEM must have at least one private key");

        builder
            .with_client_auth_cert(vec![Certificate(cert)], PrivateKey(key))
            .expect("Client auth cert")
    } else {
        builder.with_no_client_auth()
    };
    tokio_rustls::TlsConnector::from(std::sync::Arc::new(config))
}

// We box the fields to reduce type size. Without boxing the type size is 64 with native-tls and
// 1288 with native-tls. With boxing it's 16 in both. More importantly, there's a large size
// difference between the variants when using rustls, see #189.
pub(crate) enum Stream {
    TcpStream(Box<TcpStream>),
    TlsStream(Box<TlsStream<TcpStream>>),
}

#[cfg(feature = "tls-native")]
pub(crate) type TlsError = native_tls::Error;
#[cfg(feature = "tls-rustls")]
pub(crate) type TlsError = tokio_rustls::rustls::Error;

pub(crate) enum StreamError {
    TlsError(TlsError),
    IoError(std::io::Error),
}

impl From<TlsError> for StreamError {
    fn from(err: TlsError) -> Self {
        StreamError::TlsError(err)
    }
}

impl From<std::io::Error> for StreamError {
    fn from(err: std::io::Error) -> Self {
        StreamError::IoError(err)
    }
}

impl Stream {
    pub(crate) async fn new_tcp(addr: SocketAddr) -> Result<Stream, StreamError> {
        Ok(Stream::TcpStream(TcpStream::connect(addr).await?.into()))
    }

    #[cfg(feature = "tls-native")]
    pub(crate) async fn new_tls(
        addr: SocketAddr,
        host_name: &str,
        sasl: Option<&Vec<u8>>,
    ) -> Result<Stream, StreamError> {
        let tcp_stream = TcpStream::connect(addr).await?;
        // If SASL EXTERNAL is enabled create a new TLS connector with client auth cert
        let tls_stream = if sasl.is_some() {
            tls_connector(sasl).connect(host_name, tcp_stream).await?
        } else {
            TLS_CONNECTOR.connect(host_name, tcp_stream).await?
        };
        Ok(Stream::TlsStream(tls_stream.into()))
    }

    #[cfg(feature = "tls-rustls")]
    pub(crate) async fn new_tls(
        addr: SocketAddr,
        host_name: &str,
        sasl: Option<&Vec<u8>>,
    ) -> Result<Stream, StreamError> {
        use tokio_rustls::rustls::ServerName;

        let tcp_stream = TcpStream::connect(addr).await?;
        let name = ServerName::try_from(host_name).unwrap();
        // If SASL EXTERNAL is enabled create a new TLS connector with client auth cert
        let tls_stream = if sasl.is_some() {
            tls_connector(sasl).connect(name, tcp_stream).await?
        } else {
            TLS_CONNECTOR.connect(name, tcp_stream).await?
        };
        Ok(Stream::TlsStream(tls_stream.into()))
    }
}

//
// Boilerplate
//

impl AsyncRead for Stream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut ReadBuf,
    ) -> Poll<Result<(), std::io::Error>> {
        match *self {
            Stream::TcpStream(ref mut tcp_stream) => Pin::new(tcp_stream).poll_read(cx, buf),
            Stream::TlsStream(ref mut tls_stream) => Pin::new(tls_stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        match *self {
            Stream::TcpStream(ref mut tcp_stream) => Pin::new(tcp_stream).poll_write(cx, buf),
            Stream::TlsStream(ref mut tls_stream) => Pin::new(tls_stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), std::io::Error>> {
        match *self {
            Stream::TcpStream(ref mut tcp_stream) => Pin::new(tcp_stream).poll_flush(cx),
            Stream::TlsStream(ref mut tls_stream) => Pin::new(tls_stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
    ) -> Poll<Result<(), std::io::Error>> {
        match *self {
            Stream::TcpStream(ref mut tcp_stream) => Pin::new(tcp_stream).poll_shutdown(cx),
            Stream::TlsStream(ref mut tls_stream) => Pin::new(tls_stream).poll_shutdown(cx),
        }
    }
}
