use crate::config::{ClientConfig, ClientServiceConfig, Config, TransportType};
use crate::config_watcher::ServiceChange;
use crate::helper::udp_connect;
use crate::protocol::Hello::{self, *};
use crate::protocol::{
    self, read_ack, read_control_cmd, read_data_cmd, read_hello, Ack, Auth, ControlChannelCmd,
    DataChannelCmd, UdpTraffic, CURRENT_PROTO_VERSION, HASH_WIDTH_IN_BYTES,
};
use crate::transport::{TcpTransport, Transport};
use anyhow::{anyhow, bail, Context, Result};
use backoff::ExponentialBackoff;
use bytes::{Bytes, BytesMut};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{self, copy_bidirectional, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{broadcast, mpsc, oneshot, RwLock};
use tokio::time::{self, Duration};
use tracing::{debug, error, info, instrument, trace, warn, Instrument, Span};

#[cfg(feature = "noise")]
use crate::transport::NoiseTransport;
#[cfg(feature = "tls")]
use crate::transport::TlsTransport;

use crate::constants::{UDP_BUFFER_SIZE, UDP_SENDQ_SIZE, UDP_TIMEOUT};

// The entrypoint of running a client
pub async fn run_client(
    config: &Config,
    shutdown_rx: broadcast::Receiver<bool>,
    service_rx: mpsc::Receiver<ServiceChange>,
) -> Result<()> {
    let config = match &config.client {
        Some(v) => v,
        None => {
            return Err(anyhow!("Try to run as a client, but the configuration is missing. Please add the `[client]` block"))
        }
    };

    match config.transport.transport_type {
        TransportType::Tcp => {
            let mut client = Client::<TcpTransport>::from(config).await?;
            client.run(shutdown_rx, service_rx).await
        }
        TransportType::Tls => {
            #[cfg(feature = "tls")]
            {
                let mut client = Client::<TlsTransport>::from(config).await?;
                client.run(shutdown_rx, service_rx).await
            }
            #[cfg(not(feature = "tls"))]
            crate::helper::feature_not_compile("tls")
        }
        TransportType::Noise => {
            #[cfg(feature = "noise")]
            {
                let mut client = Client::<NoiseTransport>::from(config).await?;
                client.run(shutdown_rx, service_rx).await
            }
            #[cfg(not(feature = "noise"))]
            crate::helper::feature_not_compile("noise")
        }
    }
}

type ServiceDigest = protocol::Digest;
type Nonce = protocol::Digest;

// Holds the state of a client
struct Client<'a, T: Transport> {
    config: &'a ClientConfig,
    service_handles: HashMap<String, ControlChannelHandle>,
    transport: Arc<T>,
}

impl<'a, T: 'static + Transport> Client<'a, T> {
    // Create a Client from `[client]` config block
    async fn from(config: &'a ClientConfig) -> Result<Client<'a, T>> {
        Ok(Client {
            config,
            service_handles: HashMap::new(),
            transport: Arc::new(
                T::new(&config.transport)
                    .await
                    .with_context(|| "Failed to create the transport")?,
            ),
        })
    }

    // The entrypoint of Client
    async fn run(
        &mut self,
        mut shutdown_rx: broadcast::Receiver<bool>,
        mut service_rx: mpsc::Receiver<ServiceChange>,
    ) -> Result<()> {
        for (name, config) in &self.config.services {
            // Create a control channel for each service defined
            let handle = ControlChannelHandle::new(
                (*config).clone(),
                self.config.remote_addr.clone(),
                self.transport.clone(),
            );
            self.service_handles.insert(name.clone(), handle);
        }

        // Wait for the shutdown signal
        loop {
            tokio::select! {
                val = shutdown_rx.recv() => {
                    match val {
                        Ok(_) => {}
                        Err(err) => {
                            error!("Unable to listen for shutdown signal: {}", err);
                        }
                    }
                    break;
                },
                e = service_rx.recv() => {
                    if let Some(e) = e {
                        match e {
                            ServiceChange::ClientAdd(s)=> {
                                let name = s.name.clone();
                                let handle = ControlChannelHandle::new(
                                    s,
                                    self.config.remote_addr.clone(),
                                    self.transport.clone(),
                                );
                                let _ = self.service_handles.insert(name, handle);
                            },
                            ServiceChange::ClientDelete(s)=> {
                                let _ = self.service_handles.remove(&s);
                            },
                            _ => ()
                        }
                    }
                }
            }
        }

        // Shutdown all services
        for (_, handle) in self.service_handles.drain() {
            handle.shutdown();
        }

        Ok(())
    }
}

struct RunDataChannelArgs<T: Transport> {
    session_key: Nonce,
    remote_addr: String,
    local_addr: String,
    connector: Arc<T>,
}

async fn do_data_channel_handshake<T: Transport>(
    args: Arc<RunDataChannelArgs<T>>,
) -> Result<T::Stream> {
    // Retry at least every 100ms, at most for 10 seconds
    let backoff = ExponentialBackoff {
        max_interval: Duration::from_millis(100),
        max_elapsed_time: Some(Duration::from_secs(10)),
        ..Default::default()
    };

    // FIXME: Respect control channel shutdown here
    // Connect to remote_addr
    let mut conn: T::Stream = backoff::future::retry_notify(
        backoff,
        || async {
            Ok(args
                .connector
                .connect(&args.remote_addr)
                .await
                .with_context(|| "Failed to connect to remote_addr")?)
        },
        |e, duration| {
            warn!("{:?}. Retry in {:?}", e, duration);
        },
    )
    .await?;

    // Send nonce
    let v: &[u8; HASH_WIDTH_IN_BYTES] = args.session_key[..].try_into().unwrap();
    let hello = Hello::DataChannelHello(CURRENT_PROTO_VERSION, v.to_owned());
    conn.write_all(&bincode::serialize(&hello).unwrap()).await?;
    conn.flush().await?;

    Ok(conn)
}

async fn run_data_channel<T: Transport>(args: Arc<RunDataChannelArgs<T>>) -> Result<()> {
    // Do the handshake
    let mut conn = do_data_channel_handshake(args.clone()).await?;

    // Forward
    match read_data_cmd(&mut conn).await? {
        DataChannelCmd::StartForwardTcp => {
            run_data_channel_for_tcp::<T>(conn, &args.local_addr).await?;
        }
        DataChannelCmd::StartForwardUdp => {
            run_data_channel_for_udp::<T>(conn, &args.local_addr).await?;
        }
    }
    Ok(())
}

// Simply copying back and forth for TCP
#[instrument(skip(conn))]
async fn run_data_channel_for_tcp<T: Transport>(
    mut conn: T::Stream,
    local_addr: &str,
) -> Result<()> {
    debug!("New data channel starts forwarding");

    let mut local = TcpStream::connect(local_addr)
        .await
        .with_context(|| "Failed to connect to local_addr")?;
    let _ = copy_bidirectional(&mut conn, &mut local).await;
    Ok(())
}

// Things get a little tricker when it gets to UDP because it's connection-less.
// A UdpPortMap must be maintained for recent seen incoming address, giving them
// each a local port, which is associated with a socket. So just the sender
// to the socket will work fine for the map's value.
type UdpPortMap = Arc<RwLock<HashMap<SocketAddr, mpsc::Sender<Bytes>>>>;

#[instrument(skip(conn))]
async fn run_data_channel_for_udp<T: Transport>(conn: T::Stream, local_addr: &str) -> Result<()> {
    debug!("New data channel starts forwarding");

    let port_map: UdpPortMap = Arc::new(RwLock::new(HashMap::new()));

    // The channel stores UdpTraffic that needs to be sent to the server
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<UdpTraffic>(UDP_SENDQ_SIZE);

    // FIXME: https://github.com/tokio-rs/tls/issues/40
    // Maybe this is our concern
    let (mut rd, mut wr) = io::split(conn);

    // Keep sending items from the outbound channel to the server
    tokio::spawn(async move {
        while let Some(t) = outbound_rx.recv().await {
            trace!("outbound {:?}", t);
            if let Err(e) = t
                .write(&mut wr)
                .await
                .with_context(|| "Failed to forward UDP traffic to the server")
            {
                debug!("{:?}", e);
                break;
            }
        }
    });

    loop {
        // Read a packet from the server
        let hdr_len = rd.read_u8().await?;
        let packet = UdpTraffic::read(&mut rd, hdr_len)
            .await
            .with_context(|| "Failed to read UDPTraffic from the server")?;
        let m = port_map.read().await;

        if m.get(&packet.from).is_none() {
            // This packet is from a address we don't see for a while,
            // which is not in the UdpPortMap.
            // So set up a mapping (and a forwarder) for it

            // Drop the reader lock
            drop(m);

            // Grab the writer lock
            // This is the only thread that will try to grab the writer lock
            // So no need to worry about some other thread has already set up
            // the mapping between the gap of dropping the reader lock and
            // grabbing the writer lock
            let mut m = port_map.write().await;

            match udp_connect(local_addr).await {
                Ok(s) => {
                    let (inbound_tx, inbound_rx) = mpsc::channel(UDP_SENDQ_SIZE);
                    m.insert(packet.from, inbound_tx);
                    tokio::spawn(run_udp_forwarder(
                        s,
                        inbound_rx,
                        outbound_tx.clone(),
                        packet.from,
                        port_map.clone(),
                    ));
                }
                Err(e) => {
                    error!("{:?}", e);
                }
            }
        }

        // Now there should be a udp forwarder that can receive the packet
        let m = port_map.read().await;
        if let Some(tx) = m.get(&packet.from) {
            let _ = tx.send(packet.data).await;
        }
    }
}

// Run a UdpSocket for the visitor `from`
#[instrument(skip_all, fields(from))]
async fn run_udp_forwarder(
    s: UdpSocket,
    mut inbound_rx: mpsc::Receiver<Bytes>,
    outbount_tx: mpsc::Sender<UdpTraffic>,
    from: SocketAddr,
    port_map: UdpPortMap,
) -> Result<()> {
    debug!("Forwarder created");
    let mut buf = BytesMut::new();
    buf.resize(UDP_BUFFER_SIZE, 0);

    loop {
        tokio::select! {
            // Receive from the server
            data = inbound_rx.recv() => {
                if let Some(data) = data {
                    s.send(&data).await?;
                } else {
                    break;
                }
            },

            // Receive from the service
            val = s.recv(&mut buf) => {
                let len = match val {
                    Ok(v) => v,
                    Err(_) => {break;}
                };

                let t = UdpTraffic{
                    from,
                    data: Bytes::copy_from_slice(&buf[..len])
                };

                outbount_tx.send(t).await?;
            },

            // No traffic for the duration of UDP_TIMEOUT, clean up the state
            _ = time::sleep(Duration::from_secs(UDP_TIMEOUT)) => {
                break;
            }
        }
    }

    let mut port_map = port_map.write().await;
    port_map.remove(&from);

    debug!("Forwarder dropped");
    Ok(())
}

// Control channel, using T as the transport layer
struct ControlChannel<T: Transport> {
    digest: ServiceDigest,              // SHA256 of the service name
    service: ClientServiceConfig,       // `[client.services.foo]` config block
    shutdown_rx: oneshot::Receiver<u8>, // Receives the shutdown signal
    remote_addr: String,                // `client.remote_addr`
    transport: Arc<T>,                  // Wrapper around the transport layer
}

// Handle of a control channel
// Dropping it will also drop the actual control channel
struct ControlChannelHandle {
    shutdown_tx: oneshot::Sender<u8>,
}

impl<T: 'static + Transport> ControlChannel<T> {
    #[instrument(skip_all)]
    async fn run(&mut self) -> Result<()> {
        let mut conn = self
            .transport
            .connect(&self.remote_addr)
            .await
            .with_context(|| format!("Failed to connect to the server: {}", &self.remote_addr))?;

        // Send hello
        debug!("Sending hello");
        let hello_send =
            Hello::ControlChannelHello(CURRENT_PROTO_VERSION, self.digest[..].try_into().unwrap());
        conn.write_all(&bincode::serialize(&hello_send).unwrap())
            .await?;
        conn.flush().await?;

        // Read hello
        debug!("Reading hello");
        let nonce = match read_hello(&mut conn).await? {
            ControlChannelHello(_, d) => d,
            _ => {
                bail!("Unexpected type of hello");
            }
        };

        // Send auth
        debug!("Sending auth");
        let mut concat = Vec::from(self.service.token.as_ref().unwrap().as_bytes());
        concat.extend_from_slice(&nonce);

        let session_key = protocol::digest(&concat);
        let auth = Auth(session_key);
        conn.write_all(&bincode::serialize(&auth).unwrap()).await?;
        conn.flush().await?;

        // Read ack
        debug!("Reading ack");
        match read_ack(&mut conn).await? {
            Ack::Ok => {}
            v => {
                return Err(anyhow!("{}", v))
                    .with_context(|| format!("Authentication failed: {}", self.service.name));
            }
        }

        // Channel ready
        info!("Control channel established");

        let remote_addr = self.remote_addr.clone();
        let local_addr = self.service.local_addr.clone();
        let data_ch_args = Arc::new(RunDataChannelArgs {
            session_key,
            remote_addr,
            local_addr,
            connector: self.transport.clone(),
        });

        loop {
            tokio::select! {
                val = read_control_cmd(&mut conn) => {
                    let val = val?;
                    debug!( "Received {:?}", val);
                    match val {
                        ControlChannelCmd::CreateDataChannel => {
                            let args = data_ch_args.clone();
                            tokio::spawn(async move {
                                if let Err(e) = run_data_channel(args).await.with_context(|| "Failed to run the data channel") {
                                    error!("{:?}", e);
                                }
                            }.instrument(Span::current()));
                        }
                    }
                },
                _ = &mut self.shutdown_rx => {
                    break;
                }
            }
        }

        info!("Control channel shutdown");
        Ok(())
    }
}

impl ControlChannelHandle {
    #[instrument(skip_all, fields(service = %service.name))]
    fn new<T: 'static + Transport>(
        service: ClientServiceConfig,
        remote_addr: String,
        transport: Arc<T>,
    ) -> ControlChannelHandle {
        let digest = protocol::digest(service.name.as_bytes());

        info!("Starting {}", hex::encode(digest));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let mut s = ControlChannel {
            digest,
            service,
            shutdown_rx,
            remote_addr,
            transport,
        };

        tokio::spawn(
            async move {
                while let Err(err) = s
                    .run()
                    .await
                    .with_context(|| "Failed to run the control channel")
                {
                    if s.shutdown_rx.try_recv() != Err(oneshot::error::TryRecvError::Empty) {
                        break;
                    }

                    let duration = Duration::from_secs(1);
                    error!("{:?}\n\nRetry in {:?}...", err, duration);
                    time::sleep(duration).await;
                }
            }
            .instrument(Span::current()),
        );

        ControlChannelHandle { shutdown_tx }
    }

    fn shutdown(self) {
        // A send failure shows that the actor has already shutdown.
        let _ = self.shutdown_tx.send(0u8);
    }
}
