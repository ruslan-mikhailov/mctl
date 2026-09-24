use crate::recent::Endpoint;
use crate::wire::OperationDeadline;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

const IO_SLICE: Duration = Duration::from_millis(200);

pub trait ReadWrite: Read + Write {}
impl<T: Read + Write + ?Sized> ReadWrite for T {}

pub struct Connected {
    pub stream: Box<dyn ReadWrite + Send>,
    pub deadline: OperationDeadline,
}

struct CommandIo {
    socket: TcpStream,
    deadline: OperationDeadline,
    cancel: Arc<AtomicBool>,
}

impl CommandIo {
    fn wait(&self) -> io::Result<Duration> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "command cancelled",
            ));
        }
        match self.deadline.current() {
            Some(deadline) => deadline
                .checked_duration_since(Instant::now())
                .filter(|remaining| !remaining.is_zero())
                .map(|remaining| remaining.min(IO_SLICE))
                .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "command timed out")),
            None => Ok(IO_SLICE),
        }
    }
}

impl Read for CommandIo {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let wait = self.wait()?;
        self.socket.set_read_timeout(Some(wait))?;
        self.socket.read(buf)
    }
}

impl Write for CommandIo {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let wait = self.wait()?;
        self.socket.set_write_timeout(Some(wait))?;
        self.socket.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.wait()?;
        self.socket.flush()
    }
}

fn cancelled_or_expired(deadline: Instant, cancel: &AtomicBool) -> io::Result<Duration> {
    if cancel.load(Ordering::Relaxed) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "connection cancelled",
        ));
    }
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "connection timed out"))
}
// rustls may perform several successful socket operations inside one complete_io call.
// Recheck the deadline and cancellation before each one, even if a peer trickles bytes.
struct HandshakeIo<'a> {
    socket: &'a mut TcpStream,
    deadline: Instant,
    cancel: &'a AtomicBool,
}

impl Read for HandshakeIo<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let wait = cancelled_or_expired(self.deadline, self.cancel)?.min(IO_SLICE);
        self.socket.set_read_timeout(Some(wait))?;
        self.socket.read(buf)
    }
}

impl Write for HandshakeIo<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let wait = cancelled_or_expired(self.deadline, self.cancel)?.min(IO_SLICE);
        self.socket.set_write_timeout(Some(wait))?;
        self.socket.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        cancelled_or_expired(self.deadline, self.cancel)?;
        self.socket.flush()
    }
}

fn root_store(certs: Vec<CertificateDer<'static>>) -> io::Result<RootCertStore> {
    let mut roots = RootCertStore::empty();
    let (valid, _) = roots.add_parsable_certificates(certs);
    if valid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "no usable platform CA certificates",
        ));
    }
    Ok(roots)
}

fn tls_config() -> io::Result<Arc<ClientConfig>> {
    let loaded = rustls_native_certs::load_native_certs();
    // A partially loaded platform store may be missing the authority for this target.
    // Do not silently use a different set of roots after a loading error.
    if !loaded.errors.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "could not load platform CA certificates",
        ));
    }
    let roots = root_store(loaded.certs)?;
    Ok(Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
}

fn prepare(
    host: String,
    port: u16,
    tls: bool,
) -> io::Result<(Vec<SocketAddr>, Option<Arc<ClientConfig>>)> {
    let config = if tls { Some(tls_config()?) } else { None };
    let addresses = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|_| io::Error::new(io::ErrorKind::AddrNotAvailable, "could not resolve target"))?
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "target has no addresses",
        ));
    }
    Ok((addresses, config))
}

fn connect_candidates<F>(
    addresses: Vec<SocketAddr>,
    deadline: Instant,
    cancel: &AtomicBool,
    connector: F,
) -> io::Result<TcpStream>
where
    F: Fn(SocketAddr, Duration) -> io::Result<TcpStream> + Send + Sync + 'static,
{
    let connector = Arc::new(connector);
    let mut last_error = None;
    let count = addresses.len();
    for (index, address) in addresses.into_iter().enumerate() {
        let wait = cancelled_or_expired(deadline, cancel)? / (count - index) as u32;
        let attempt_deadline = Instant::now() + wait;
        let (sender, receiver) = mpsc::sync_channel(1);
        let connector = connector.clone();
        thread::Builder::new()
            .name("mctl-tcp-connect".into())
            .spawn(move || {
                let _ = sender.send(connector(address, wait));
            })?;
        loop {
            let remaining = cancelled_or_expired(deadline, cancel)?;
            let Some(candidate_remaining) = attempt_deadline.checked_duration_since(Instant::now())
            else {
                last_error = Some(io::Error::from(io::ErrorKind::TimedOut));
                break;
            };
            match receiver.recv_timeout(remaining.min(candidate_remaining).min(IO_SLICE)) {
                Ok(Ok(stream)) => {
                    cancelled_or_expired(deadline, cancel)?;
                    return Ok(stream);
                }
                Ok(Err(error)) => {
                    last_error = Some(error);
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::other("TCP connection worker failed"));
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| io::Error::other("connection failed")))
}

fn connect_before(
    endpoint: &Endpoint,
    deadline: Instant,
    cancel: &Arc<AtomicBool>,
) -> io::Result<Connected> {
    cancelled_or_expired(deadline, cancel)?;
    if endpoint.port == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "port must be nonzero",
        ));
    }
    // This validation rejects malformed names (including embedded credentials) before DNS.
    // It also supplies the exact hostname/IP for rustls's certificate validation.
    let server_name = ServerName::try_from(endpoint.host.clone())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid target hostname"))?;

    // System DNS and native certificate-store lookups have no standard cancellable API.
    // Run them off-thread so the caller can still observe its deadline and Ctrl-C.
    let (sender, receiver) = mpsc::sync_channel(1);
    let host = endpoint.host.clone();
    let port = endpoint.port;
    let tls = endpoint.tls;
    thread::Builder::new()
        .name("mctl-connection-setup".into())
        .spawn(move || {
            let _ = sender.send(prepare(host, port, tls));
        })?;
    let (addresses, config) = loop {
        let wait = cancelled_or_expired(deadline, cancel)?.min(IO_SLICE);
        match receiver.recv_timeout(wait) {
            Ok(result) => break result?,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(io::Error::other("connection setup failed"));
            }
        }
    };
    cancelled_or_expired(deadline, cancel)?;

    let mut socket = connect_candidates(addresses, deadline, cancel, |address, wait| {
        TcpStream::connect_timeout(&address, wait)
    })?;
    cancelled_or_expired(deadline, cancel)?;
    socket.set_read_timeout(Some(IO_SLICE))?;
    socket.set_write_timeout(Some(IO_SLICE))?;

    if let Some(config) = config {
        let mut connection = ClientConnection::new(config, server_name)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        // StreamOwned can otherwise be returned before certificate verification completes.
        while connection.is_handshaking() {
            let mut io = HandshakeIo {
                socket: &mut socket,
                deadline,
                cancel,
            };
            match connection.complete_io(&mut io) {
                Ok((0, 0)) if connection.is_handshaking() => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "TLS handshake ended early",
                    ));
                }
                Ok(_) => (),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) =>
                {
                    ()
                }
                Err(error) => return Err(error),
            }
        }
        cancelled_or_expired(deadline, cancel)?;
        let control = OperationDeadline::default();
        Ok(Connected {
            stream: Box::new(StreamOwned::new(
                connection,
                CommandIo {
                    socket,
                    deadline: control.clone(),
                    cancel: cancel.clone(),
                },
            )),
            deadline: control,
        })
    } else {
        cancelled_or_expired(deadline, cancel)?;
        Ok(Connected {
            stream: Box::new(socket),
            deadline: OperationDeadline::default(),
        })
    }
}

pub fn connect(
    endpoint: &Endpoint,
    timeout: Duration,
    cancel: &Arc<AtomicBool>,
) -> io::Result<Connected> {
    let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "connection timeout too large")
    })?;
    connect_before(endpoint, deadline, cancel)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(host: &str, tls: bool) -> Endpoint {
        Endpoint {
            host: host.into(),
            port: 11211,
            tls,
        }
    }

    #[test]
    fn invalid_hostname_is_rejected_before_resolution_or_tls() {
        let error = connect(
            &endpoint("user:secret@host", true),
            Duration::from_secs(1),
            &Arc::new(AtomicBool::new(false)),
        )
        .err()
        .expect("invalid hostname must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!error.to_string().contains("secret"));
    }

    #[test]
    fn empty_roots_are_not_accepted() {
        let error = root_store(Vec::new()).expect_err("empty CA store must fail closed");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn expired_deadline_is_rejected_before_resolution() {
        let error = connect_before(
            &endpoint("localhost", false),
            Instant::now() - Duration::from_secs(1),
            &Arc::new(AtomicBool::new(false)),
        )
        .err()
        .expect("past deadline must fail");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn cancelled_connection_is_rejected_before_resolution() {
        let error = connect(
            &endpoint("localhost", true),
            Duration::from_secs(1),
            &Arc::new(AtomicBool::new(true)),
        )
        .err()
        .expect("cancelled connection must fail");
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn slow_connect_is_not_restarted_every_poll_interval() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let error = connect_candidates(
            vec![address],
            Instant::now() + Duration::from_millis(750),
            &AtomicBool::new(false),
            |address, wait| {
                if wait < Duration::from_millis(300) {
                    thread::sleep(wait);
                    Err(io::ErrorKind::TimedOut.into())
                } else {
                    thread::sleep(Duration::from_millis(300));
                    TcpStream::connect(address)
                }
            },
        )
        .err();
        assert!(
            error.is_none(),
            "300ms connection must fit within 750ms deadline: {error:?}"
        );
    }
    #[test]
    fn tls_record_trickle_obeys_command_deadline() {
        use crate::wire::{ResponseKind, Wire};
        use rustls::pki_types::PrivateKeyDer;
        use rustls::{ServerConfig, ServerConnection};
        use std::net::TcpListener;

        let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let certificate = certified.cert.der().clone();
        let mut roots = RootCertStore::empty();
        roots.add(certificate.clone()).unwrap();
        let server_config = Arc::new(
            ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(
                    vec![certificate],
                    PrivateKeyDer::Pkcs8(certified.signing_key.serialize_der().into()),
                )
                .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            let mut server =
                StreamOwned::new(ServerConnection::new(server_config).unwrap(), socket);
            let mut request = [0; 9];
            server.read_exact(&mut request).unwrap();
            assert_eq!(&request, b"version\r\n");
            server.conn.writer().write_all(b"VERSION 1\r\n").unwrap();
            let mut record = Vec::new();
            server.conn.write_tls(&mut record).unwrap();
            for byte in record.iter().take(30) {
                if server.sock.write_all(&[*byte]).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(40));
            }
        });
        let mut socket = TcpStream::connect(address).unwrap();
        socket.set_read_timeout(Some(IO_SLICE)).unwrap();
        socket.set_write_timeout(Some(IO_SLICE)).unwrap();
        let mut client = ClientConnection::new(
            Arc::new(
                ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth(),
            ),
            ServerName::try_from("localhost").unwrap(),
        )
        .unwrap();
        while client.is_handshaking() {
            client.complete_io(&mut socket).unwrap();
        }
        let control = OperationDeadline::default();
        let stream = CommandIo {
            socket,
            deadline: control.clone(),
            cancel: Arc::new(AtomicBool::new(false)),
        };
        let mut wire = Wire::with_deadline(
            StreamOwned::new(client, stream),
            Duration::from_millis(350),
            control,
        );
        let started = Instant::now();
        let error = wire
            .execute(
                b"version\r\n",
                ResponseKind::Version,
                &AtomicBool::new(false),
            )
            .unwrap_err();
        let elapsed = started.elapsed();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            elapsed < Duration::from_millis(800),
            "TLS record held the command for {elapsed:?}"
        );
        server.join().unwrap();
    }
}
