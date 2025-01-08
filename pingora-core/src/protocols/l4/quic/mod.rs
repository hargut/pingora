use std::collections::HashMap;
use std::{io, mem};
use std::fmt::{Debug, Formatter};
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, RawFd};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use log::{debug, error, trace, warn};
use parking_lot::Mutex;
use quiche::{Config, ConnectionId, Header, RecvInfo, Stats, Type};
use ring::hmac::Key;
use ring::rand::SystemRandom;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::UdpSocket;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::{mpsc, Notify};
use pingora_error::{BError, Error, ErrorType, Result};
use quiche::Connection as QuicheConnection;
use tokio::task::JoinHandle;
use settings::Settings as QuicSettings;

mod sendto;
mod id_token;
pub(crate) mod tls_handshake;
mod settings;

use crate::protocols::ConnectionState;
use crate::protocols::l4::quic::sendto::{detect_gso, send_to, set_txtime_sockopt};
use crate::protocols::l4::stream::Stream as L4Stream;

// UDP header 8 bytes, IPv4 Header 20 bytes
//pub const MAX_IPV4_BUF_SIZE: usize = 65507;
// UDP header 8 bytes, IPv6 Header 40 bytes
pub const MAX_IPV6_BUF_SIZE: usize = 65487;

// 1500(Ethernet) - 20(IPv4 header) - 8(UDP header) = 1472.
//pub const MAX_IPV4_UDP_PACKET_SIZE: usize = 1472;
// 1500(Ethernet) - 40(IPv6 header) - 8(UDP header) = 1452
pub const MAX_IPV6_UDP_PACKET_SIZE: usize = 1452;

//pub const MAX_IPV4_QUIC_DATAGRAM_SIZE: usize = 1370;
pub const MAX_IPV6_QUIC_DATAGRAM_SIZE: usize = 1350;

const HANDSHAKE_PACKET_BUFFER_SIZE: usize = 64;
const CONNECTION_DROP_CHANNEL_SIZE : usize = 1024;

pub struct Listener {
    socket: Arc<UdpSocket>,
    socket_details: SocketDetails,

    config: Arc<Mutex<Config>>,
    crypto: Crypto,

    connections: Mutex<HashMap<ConnectionId<'static>, ConnectionHandle>>,
    drop_connections: (Sender<ConnectionId<'static>>, Mutex<Receiver<ConnectionId<'static>>>)
}

pub struct Crypto {
    key: Key,
}

pub enum Connection {
    Incoming(IncomingState),
    Established(EstablishedState),
}

pub struct IncomingState {
    connection_id: ConnectionId<'static>,
    config: Arc<Mutex<Config>>,
    drop_connection: Sender<ConnectionId<'static>>,

    socket: Arc<UdpSocket>,
    socket_details: SocketDetails,
    udp_rx: Receiver<UdpRecv>,
    response_tx: Sender<HandshakeResponse>,

    dgram: UdpRecv,

    ignore: bool,
    reject: bool
}

#[derive(Clone)]
struct SocketDetails {
    addr: SocketAddr,
    gso_enabled: bool,
    pacing_enabled: bool,
}

pub struct EstablishedState {
    socket: Arc<UdpSocket>,
    tx_handle: JoinHandle<Result<()>>,

    pub(crate) connection_id: ConnectionId<'static>,
    pub connection: Arc<Mutex<QuicheConnection>>,
    pub drop_connection: Sender<ConnectionId<'static>>,
    pub rx_notify: Arc<Notify>,
    pub tx_notify: Arc<Notify>,
    pub tx_flushed: Arc<Notify>,
}

pub enum ConnectionHandle {
    Incoming(IncomingHandle),
    Established(EstablishedHandle),
}

impl Debug for ConnectionHandle {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConnectionHandle")?;
        match self {
            ConnectionHandle::Incoming(_) => f.write_str("::Incoming"),
            ConnectionHandle::Established(_) => f.write_str("::Established"),
        }
    }
}

pub struct IncomingHandle {
    udp_tx: Sender<UdpRecv>,
    response_rx: Receiver<HandshakeResponse>,
}

pub(crate) enum HandshakeResponse {
    Established(EstablishedHandle),
    Ignored,
    Rejected,
    // TODO: TimedOut,
}

#[derive(Clone)]
pub struct EstablishedHandle {
    connection_id: ConnectionId<'static>,
    connection: Arc<Mutex<QuicheConnection>>,
    rx_notify: Arc<Notify>,
    tx_notify: Arc<Notify>,
}

pub struct UdpRecv {
    pub(crate) pkt: Vec<u8>,
    pub(crate) header: Header<'static>,
    pub(crate) recv_info: RecvInfo,
}

impl TryFrom<UdpSocket> for Listener {
    type Error = BError;

    fn try_from(io: UdpSocket) -> Result<Self, Self::Error> {
        let addr = io.local_addr()
            .map_err(|e| Error::explain(
                ErrorType::SocketError,
                format!("failed to get local address from socket: {}", e)))?;
        let rng = SystemRandom::new();
        let key = Key::generate(ring::hmac::HMAC_SHA256, &rng)
            .map_err(|e| Error::explain(
                ErrorType::InternalError,
                format!("failed to generate listener key: {}", e)))?;

        let settings = QuicSettings::try_default()?;

        let gso_enabled = detect_gso(&io, MAX_IPV6_QUIC_DATAGRAM_SIZE);
        let pacing_enabled = match set_txtime_sockopt(&io) {
            Ok(_) => {
                debug!("successfully set SO_TXTIME socket option");
                true
            },
            Err(e) => {
                debug!("setsockopt failed {:?}", e);
                false
            },
        };

        let drop_connections = mpsc::channel(CONNECTION_DROP_CHANNEL_SIZE);
        Ok(Listener {
            socket: Arc::new(io),
            socket_details: SocketDetails {
                addr,
                gso_enabled,
                pacing_enabled,
            },

            config: settings.get_config(),
            crypto: Crypto {
                key
            },

            connections: Default::default(),
            drop_connections: (drop_connections.0, Mutex::new(drop_connections.1))
        })
    }
}

impl Listener {
    pub(crate) async fn accept(&self) -> io::Result<(L4Stream, SocketAddr)> {
        let mut rx_buf = [0u8; MAX_IPV6_BUF_SIZE];

        debug!("endpoint rx loop");
        'read: loop {
            // receive from network and parse Quic header
            let (size, from) = self.socket.recv_from(&mut rx_buf).await?;

            // cleanup connections
            {
                let mut drop_conn = self.drop_connections.1.lock();
                let mut conn = self.connections.lock();
                'housekeep: loop {
                    match drop_conn.try_recv() {
                        Ok(drop_id) => {
                            match conn.remove(&drop_id) {
                                None => error!("failed to remove connection handle {:?}", drop_id),
                                Some(_) => debug!("removed connection handle {:?} from connections", drop_id)
                            }
                        }
                        Err(e) => match e {
                            TryRecvError::Empty => break 'housekeep,
                            TryRecvError::Disconnected => {
                                debug_assert!(false, "drop connections receiver disconnected");
                                break 'housekeep
                            }
                        }
                    };
                }
            }

            // parse the Quic packet's header
            let header = match Header::from_slice(rx_buf[..size].as_mut(), quiche::MAX_CONN_ID_LEN) {
                Ok(hdr) => hdr,
                Err(e) => {
                    warn!("Parsing Quic packet header failed with error: {:?}.", e);
                    trace!("Dropped packet due to invalid header. Continuing...");
                    continue 'read;
                }
            };

            // TODO: allow for connection id updates during lifetime
            // connection needs to be able to update source_ids() or destination_ids()

            let recv_info = RecvInfo {
                to: self.socket_details.addr,
                from,
            };

            let mut conn_id = header.dcid.clone();
            let mut udp_tx = None;
            {
                let mut connections = self.connections.lock();
                // send to corresponding connection
                let mut handle;
                handle = connections.get_mut(&conn_id);
                if handle.is_none() {
                    conn_id = Self::gen_cid(&self.crypto.key, &header);
                    handle = connections.get_mut(&conn_id);
                };

                trace!("connection {:?} dgram received from={} length={}", conn_id, from, size);

                if let Some(handle) = handle {
                    debug!("existing connection {:?} {:?} {:?}", conn_id, handle, header);
                    match handle {
                        ConnectionHandle::Incoming(i) => {
                            match i.response_rx.try_recv() {
                                Ok(msg) => {
                                    match msg {
                                        HandshakeResponse::Established(e) => {
                                            debug!("received HandshakeResponse::Established");
                                            // receive data into existing connection
                                            match Self::recv_connection(e.connection.as_ref(), &mut rx_buf[..size], recv_info) {
                                                Ok(_len) => {
                                                    e.rx_notify.notify_waiters();
                                                    e.tx_notify.notify_waiters();
                                                    // transition connection
                                                    handle.establish(e);
                                                    continue 'read;
                                                }
                                                Err(e) => {
                                                    // TODO: take action on errors, e.g close connection, send & remove
                                                    break 'read Err(e);
                                                }
                                            }
                                        }
                                        HandshakeResponse::Ignored
                                        | HandshakeResponse::Rejected => {
                                            connections.remove(&header.dcid);
                                            continue 'read
                                        }
                                    }
                                }
                                Err(e) => {
                                    match e {
                                        TryRecvError::Empty => {
                                            udp_tx = Some(i.udp_tx.clone());
                                        }
                                        TryRecvError::Disconnected => {
                                            warn!("dropping connection {:?} handshake response channel receiver disconnected.", &header.dcid);
                                            connections.remove(&header.dcid);
                                        }
                                    };
                                }
                            }
                        }
                        ConnectionHandle::Established(e) => {
                            // receive data into existing connection
                            match Self::recv_connection(e.connection.as_ref(), &mut rx_buf[..size], recv_info) {
                                Ok(_len) => {
                                    e.rx_notify.notify_waiters();
                                    e.tx_notify.notify_waiters();
                                    continue 'read;
                                }
                                Err(e) => {
                                    // TODO: take action on errors, e.g close connection, send & remove
                                    break 'read Err(e);
                                }
                            }
                        }
                    }
                }
            };
            if let Some(udp_tx) = udp_tx {
                // receive data on UDP channel
                match udp_tx.send(UdpRecv {
                    pkt: rx_buf[..size].to_vec(),
                    header,
                    recv_info,
                }).await {
                    Ok(()) => {},
                    Err(e) => warn!("sending dgram to connection {:?} failed with error: {}", conn_id, e)
                }
                continue 'read;
            }


            if header.ty != Type::Initial {
                debug!("Quic packet type is not \"Initial\". Header: {:?}. Continuing...", header);
                continue 'read;
            }

            // create incoming connection & handle
            let (udp_tx, udp_rx) = channel::<UdpRecv>(HANDSHAKE_PACKET_BUFFER_SIZE);
            let (response_tx, response_rx) = channel::<HandshakeResponse>(1);

            debug!("new incoming connection {:?}", conn_id);
            let connection = Connection::Incoming(IncomingState {
                connection_id: conn_id.clone(),
                config: self.config.clone(),
                drop_connection: self.drop_connections.0.clone(),

                socket: self.socket.clone(),
                socket_details: self.socket_details.clone(),
                udp_rx,
                response_tx,

                dgram: UdpRecv {
                    pkt: rx_buf[..size].to_vec(),
                    header,
                    recv_info,
                },

                ignore: false,
                reject: false,
            });
            let handle = ConnectionHandle::Incoming(IncomingHandle {
                udp_tx,
                response_rx,
            });

            {
                let mut connections = self.connections.lock();
                connections.insert(conn_id, handle);
            }

            return Ok((connection.into(), from))
        }
    }

    fn recv_connection(conn: &Mutex<QuicheConnection>, mut rx_buf: &mut [u8], recv_info: RecvInfo) -> io::Result<usize> {
        let size = rx_buf.len();
        let mut conn = conn.lock();
        match conn.recv(&mut rx_buf, recv_info) {
            Ok(len) => {
                debug!("connection received: length={}", len);
                debug_assert_eq!(size, len, "size received on connection not equal to len received from network.");
                Ok(len)
            }
            Err(e) => {
                error!("connection receive error: {:?}", e);
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    format!("Connection could not receive network data for {:?}. {:?}",
                            conn.destination_id(), e)))
            }
        }
    }

    fn gen_cid(key: &Key, hdr: &Header) -> ConnectionId<'static> {
        let conn_id = ring::hmac::sign(key, &hdr.dcid);
        let conn_id = conn_id.as_ref()[..quiche::MAX_CONN_ID_LEN].to_vec();
        let conn_id = ConnectionId::from(conn_id);
        trace!("generated connection id {:?}", conn_id);
        conn_id
    }

    pub(super) fn get_raw_fd(&self) -> RawFd {
        self.socket.as_raw_fd()
    }
}

impl ConnectionHandle {
    fn establish(&mut self, handle: EstablishedHandle) {
        match self {
            ConnectionHandle::Incoming(_) => {
                debug!("connection handle {:?} established", handle.connection_id);
                let _ = mem::replace(self, ConnectionHandle::Established(handle));
            }
            ConnectionHandle::Established(_) => {}
        }
    }
}

impl Connection {
    fn establish(&mut self, state: EstablishedState) -> Result<()> {
        if cfg!(test) {
            let conn = state.connection.lock();
            debug_assert!(conn.is_established() || conn.is_in_early_data(),
                          "connection must be established or ready for data")
        }
        match self {
            Connection::Incoming(s) => {
                debug_assert!(s.udp_rx.is_empty(),
                              "udp rx channel must be empty when establishing the connection");
                debug!("connection {:?} established", state.connection_id);
                let _ = mem::replace(self, Connection::Established(state));
                Ok(())
            }
            Connection::Established(_) => Err(Error::explain(
                ErrorType::InternalError,
                "establishing connection only possible on incoming connection"))
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        match self {
            Connection::Incoming(_) => {}
            Connection::Established(s) => {
                if !s.tx_handle.is_finished() {
                    s.tx_handle.abort();
                    error!("stopped connection tx task");
                }
            }
        }
    }
}

struct ConnectionTx {
    socket: Arc<UdpSocket>,
    socket_details: SocketDetails,

    connection: Arc<Mutex<QuicheConnection>>,
    connection_id: ConnectionId<'static>,

    tx_notify: Arc<Notify>,
    tx_flushed: Arc<Notify>,
    tx_stats: TxBurst,
}

impl ConnectionTx {
    async fn start_tx(mut self) -> Result<()> {
        let id = self.connection_id;
        let mut out = [0u8;MAX_IPV6_BUF_SIZE];

        let mut finished_sending = false;
        debug!("connection {:?} tx write", id);
        'write: loop {
            // update stats from connection
            let max_send_burst = {
                let conn = self.connection.lock();
                self.tx_stats.max_send_burst(conn.stats(), conn.send_quantum())
            };
            let mut total_write = 0;
            let mut dst_info = None;

            // fill tx buffer with connection data
            trace!("connection {:?} total_write={}, max_send_burst={}", id, total_write, max_send_burst);
            'fill: while total_write < max_send_burst {
                let send = {
                    let mut conn = self.connection.lock();
                    conn.send(&mut out[total_write..max_send_burst])
                };

                let (size, send_info) = match send {
                    Ok((size, info)) => {
                        debug!("connection {:?} sent to={:?}, length={}", id, info.to, size);
                        (size, info)
                    },
                    Err(e) => {
                        if e == quiche::Error::Done {
                            trace!("connection {:?} send finished", id);
                            finished_sending = true;
                            break 'fill;
                        }
                        error!("connection {:?} send error: {:?}", id, e);
                        /* TODO: close connection
                            let mut conn = self.connection.lock();
                            conn.close(false, 0x1, b"fail").ok();
                         */
                        break 'write Err(Error::explain(
                            ErrorType::WriteError,
                            format!("Connection {:?} send data to network failed with {:?}", id, e)));
                    }
                };

                total_write += size;
                // Use the first packet time to send, not the last.
                let _ = dst_info.get_or_insert(send_info);
            }

            if total_write == 0 || dst_info.is_none() {
                debug!("connection {:?} nothing to send, waiting for notification...", id);
                self.tx_notify.notified().await;
                continue;
            }
            let dst_info = dst_info.unwrap();

            // send to network
            if let Err(e) = send_to(
                &self.socket,
                &out[..total_write],
                &dst_info,
                self.tx_stats.max_datagram_size,
                self.socket_details.pacing_enabled,
                self.socket_details.gso_enabled,
            ).await {
                if e.kind() == io::ErrorKind::WouldBlock {
                    error!("connection {:?} network socket would block", id);
                    continue
                }
                break 'write Err(Error::explain(
                    ErrorType::WriteError,
                    format!("connection {:?} network send failed with {:?}", id, e)));
            }
            trace!("connection {:?} network sent to={} bytes={}", id, dst_info.to, total_write);

            if finished_sending {
                // used during connection shutdown
                self.tx_flushed.notify_waiters();
                self.tx_notify.notified().await
            }
        }
    }
}

pub struct TxBurst {
    loss_rate: f64,
    max_send_burst: usize,
    max_datagram_size: usize
}

impl TxBurst {
    fn new(max_send_udp_payload_size: usize) -> Self {
        Self {
            loss_rate: 0.0,
            max_send_burst: MAX_IPV6_BUF_SIZE,
            max_datagram_size: max_send_udp_payload_size,
        }
    }

    fn max_send_burst(&mut self, stats: Stats, send_quantum: usize) -> usize {
        // Reduce max_send_burst by 25% if loss is increasing more than 0.1%.
        let loss_rate = stats.lost as f64 / stats.sent as f64;

        if loss_rate > self.loss_rate + 0.001 {
            self.max_send_burst = self.max_send_burst / 4 * 3;
            // Minimum bound of 10xMSS.
            self.max_send_burst =
                self.max_send_burst.max(self.max_datagram_size * 10);
            self.loss_rate = loss_rate;
        }

        send_quantum.min(self.max_send_burst) /
            self.max_datagram_size * self.max_datagram_size
    }
}

impl AsRawFd for Connection {
    fn as_raw_fd(&self) -> RawFd {
        match self {
            Connection::Incoming(s) => s.socket.as_raw_fd(),
            Connection::Established(s) => s.socket.as_raw_fd()
        }
    }
}

impl Debug for Listener {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listener")
            .field("io", &self.socket)
            .finish()
    }
}


impl Connection {
    pub(crate) fn local_addr(&self) -> io::Result<SocketAddr> {
        match self {
            Connection::Incoming(s) => s.socket.local_addr(),
            Connection::Established(s) => s.socket.local_addr()
        }
    }
}

impl Debug for Connection {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuicConnection").finish()
    }
}

#[allow(unused_variables)] // TODO: remove
impl AsyncWrite for Connection {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        todo!()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        // FIXME: this is called on l4::Stream::drop()
        // correlates to the connection, check if stopping tx loop for connection & final flush is feasible
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        todo!()
    }
}

#[allow(unused_variables)] // TODO: remove
impl AsyncRead for Connection {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        todo!()
    }
}

impl ConnectionState for Connection {
    fn quic_connection_state(&mut self) -> Option<&mut Connection> {
        Some(self)
    }

    fn is_quic_connection(&self) -> bool {
        true
    }
}
