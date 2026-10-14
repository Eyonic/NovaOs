//! TLS 1.2/1.3 client streams: rustls (no_std, unbuffered API) with the RustCrypto
//! provider, verifying server certificates against the Mozilla root store
//! (webpki-roots) and the real-time clock.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::time::Duration;

use rustls::client::UnbufferedClientConnection;
use rustls::pki_types::{ServerName, UnixTime};
use rustls::unbuffered::{ConnectionState, EncodeError, EncryptError, UnbufferedStatus};
use rustls::{ClientConfig, RootCertStore};

use crate::net::TcpStream;
use crate::sync::SpinLock;

#[derive(Debug)]
struct RtcTime;

impl rustls::time_provider::TimeProvider for RtcTime {
    fn current_time(&self) -> Option<UnixTime> {
        Some(UnixTime::since_unix_epoch(Duration::from_secs(crate::rtc::unix_time())))
    }
}

static CONFIG: SpinLock<Option<Arc<ClientConfig>>> = SpinLock::new(None);

fn config() -> Result<Arc<ClientConfig>, String> {
    let mut guard = CONFIG.try_lock().ok_or("TLS config busy")?;
    if let Some(c) = guard.as_ref() {
        return Ok(c.clone());
    }
    let roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let config = ClientConfig::builder_with_details(Arc::new(rustls_rustcrypto::provider()), Arc::new(RtcTime))
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS setup: {e}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let config = Arc::new(config);
    *guard = Some(config.clone());
    Ok(config)
}

pub struct TlsStream {
    tcp: TcpStream,
    conn: UnbufferedClientConnection,
    incoming: Vec<u8>,
    used: usize,
    outgoing: Vec<u8>,
    plaintext: VecDeque<u8>,
    peer_closed: bool,
}

#[derive(PartialEq)]
enum Goal<'a> {
    Handshake,
    Write(&'a [u8]),
    Read,
}

impl TlsStream {
    pub fn connect(tcp: TcpStream, host: &str) -> Result<Self, String> {
        let name = ServerName::try_from(host.to_string()).map_err(|_| format!("invalid TLS server name {host}"))?;
        let conn = UnbufferedClientConnection::new(config()?, name).map_err(|e| format!("TLS: {e}"))?;
        let mut stream = Self {
            tcp,
            conn,
            incoming: vec![0; 32 * 1024],
            used: 0,
            outgoing: Vec::new(),
            plaintext: VecDeque::new(),
            peer_closed: false,
        };
        stream.drive(Goal::Handshake)?;
        Ok(stream)
    }

    /// Run the rustls state machine until `goal` is reached.
    fn drive(&mut self, goal: Goal) -> Result<(), String> {
        let mut written = false;
        loop {
            let Self { tcp, conn, incoming, used, outgoing, plaintext, peer_closed } = self;
            let UnbufferedStatus { mut discard, state } = conn.process_tls_records(&mut incoming[..*used]);
            let state = state.map_err(|e| format!("TLS error: {e}"))?;
            let mut need_data = false;
            let mut done = false;
            match state {
                ConnectionState::ReadTraffic(mut traffic) => {
                    while let Some(record) = traffic.next_record() {
                        let record = record.map_err(|e| format!("TLS error: {e}"))?;
                        discard += record.discard;
                        plaintext.extend(record.payload);
                    }
                }
                ConnectionState::EncodeTlsData(mut enc) => {
                    let start = outgoing.len();
                    outgoing.resize(start + 4096, 0);
                    let n = match enc.encode(&mut outgoing[start..]) {
                        Ok(n) => n,
                        Err(EncodeError::InsufficientSize(s)) => {
                            outgoing.resize(start + s.required_size, 0);
                            enc.encode(&mut outgoing[start..]).map_err(|e| format!("TLS encode: {e}"))?
                        }
                        Err(e) => return Err(format!("TLS encode: {e}")),
                    };
                    outgoing.truncate(start + n);
                }
                ConnectionState::TransmitTlsData(tx) => {
                    tcp.write_all(outgoing)?;
                    outgoing.clear();
                    tx.done();
                }
                ConnectionState::BlockedHandshake => need_data = true,
                ConnectionState::WriteTraffic(mut traffic) => match goal {
                    Goal::Handshake => done = true,
                    Goal::Write(data) if !written => {
                        let mut buf = vec![0; data.len() + 1024];
                        let n = match traffic.encrypt(data, &mut buf) {
                            Ok(n) => n,
                            Err(EncryptError::InsufficientSize(s)) => {
                                buf.resize(s.required_size, 0);
                                traffic.encrypt(data, &mut buf).map_err(|e| format!("TLS encrypt: {e}"))?
                            }
                            Err(e) => return Err(format!("TLS encrypt: {e}")),
                        };
                        tcp.write_all(&buf[..n])?;
                        written = true;
                        done = true;
                    }
                    Goal::Write(_) => done = true,
                    Goal::Read if !plaintext.is_empty() || *peer_closed => done = true,
                    Goal::Read => need_data = true,
                },
                ConnectionState::PeerClosed | ConnectionState::Closed => {
                    *peer_closed = true;
                    if goal == Goal::Read {
                        done = true;
                    } else if !matches!(goal, Goal::Write(_)) || !written {
                        return Err("TLS connection closed by peer".into());
                    }
                }
                _ => return Err("TLS: unexpected connection state".into()),
            }
            // `state` borrowed `incoming`; it is dropped by now, so discard consumed bytes.
            if discard > 0 {
                incoming.copy_within(discard..*used, 0);
                *used -= discard;
            }
            if done {
                return Ok(());
            }
            if need_data {
                if *used == incoming.len() {
                    incoming.resize(incoming.len() * 2, 0);
                }
                let n = tcp.read(&mut incoming[*used..])?;
                if n == 0 {
                    // Closed without close_notify: treat as end of stream when reading.
                    if goal == Goal::Read {
                        *peer_closed = true;
                        return Ok(());
                    }
                    return Err("connection closed during TLS handshake".into());
                }
                *used += n;
            }
        }
    }
}

impl crate::http::Stream for TlsStream {
    fn write_all(&mut self, data: &[u8]) -> Result<(), String> {
        for chunk in data.chunks(16 * 1024) {
            self.drive(Goal::Write(chunk))?;
        }
        Ok(())
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize, String> {
        if self.plaintext.is_empty() && !self.peer_closed {
            self.drive(Goal::Read)?;
        }
        let n = buf.len().min(self.plaintext.len());
        for (dst, src) in buf.iter_mut().zip(self.plaintext.drain(..n)) {
            *dst = src;
        }
        Ok(n)
    }
}
