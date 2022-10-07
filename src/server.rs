//! The heart of the server.

use std::error::Error;
use std::io::Read;
use std::iter::FusedIterator;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{io, thread};

use anyhow::{bail, ensure, Context};
use byteorder::{BigEndian, ReadBytesExt};
use flume::{Receiver, Sender};
use hmac::{Hmac, Mac};
use num::BigInt;
use rand::rngs::OsRng;
use rayon::iter::ParallelIterator;
use reqwest::{Client as HttpClient, StatusCode};
use rsa::{PaddingScheme, PublicKeyParts, RsaPrivateKey};
use serde::Deserialize;
use serde_json::{json, Value};
use sha1::digest::Update;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::{Handle, Runtime};
use tokio::sync::{oneshot, Semaphore};
use uuid::Uuid;
use valence_nbt::{compound, Compound, List};

use crate::biome::{validate_biomes, Biome, BiomeId};
use crate::client::{Client, Clients};
use crate::config::{Config, ConnectionMode, ServerListPing};
use crate::dimension::{validate_dimensions, Dimension, DimensionId};
use crate::entity::Entities;
use crate::ident::Ident;
use crate::player_list::PlayerLists;
use crate::player_textures::SignedPlayerTextures;
use crate::protocol::codec::{Decoder, Encoder};
use crate::protocol::packets::c2s::handshake::{Handshake, HandshakeNextState};
use crate::protocol::packets::c2s::login::{
    EncryptionResponse, LoginPluginResponse, LoginStart, VerifyTokenOrMsgSig,
};
use crate::protocol::packets::c2s::play::C2sPlayPacket;
use crate::protocol::packets::c2s::status::{PingRequest, StatusRequest};
use crate::protocol::packets::s2c::login::{
    DisconnectLogin, EncryptionRequest, LoginPluginRequest, LoginSuccess, SetCompression,
};
use crate::protocol::packets::s2c::play::S2cPlayPacket;
use crate::protocol::packets::s2c::status::{PingResponse, StatusResponse};
use crate::protocol::packets::Property;
use crate::protocol::{BoundedArray, BoundedString, Decode, RawBytes, VarInt};
use crate::proxy::velocity::{
    VELOCITY_MAX_SUPPORTED_VERSION, VELOCITY_PLAYER_INFO_CHANNEL, VELOCITY_SUPPORTED_VERSION,
};
use crate::text::Text;
use crate::util::valid_username;
use crate::world::Worlds;
use crate::{ident, Ticks, PROTOCOL_VERSION, VERSION_NAME};

/// Contains the entire state of a running Minecraft server, accessible from
/// within the [update](crate::config::Config::update) loop.
pub struct Server<C: Config> {
    /// Custom state.
    pub state: C::ServerState,
    /// A handle to this server's [`SharedServer`].
    pub shared: SharedServer<C>,
    /// All of the clients on the server.
    pub clients: Clients<C>,
    /// All of entities on the server.
    pub entities: Entities<C>,
    /// All of the worlds on the server.
    pub worlds: Worlds<C>,
    /// All of the player lists on the server.
    pub player_lists: PlayerLists<C>,
}

/// A handle to a Minecraft server containing the subset of functionality which
/// is accessible outside the [update][update] loop.
///
/// `SharedServer`s are internally refcounted and can
/// be shared between threads.
///
/// [update]: crate::config::Config::update
pub struct SharedServer<C: Config>(Arc<SharedServerInner<C>>);

impl<C: Config> Clone for SharedServer<C> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

struct SharedServerInner<C: Config> {
    cfg: C,
    address: SocketAddr,
    tick_rate: Ticks,
    connection_mode: ConnectionMode,
    max_connections: usize,
    incoming_packet_capacity: usize,
    outgoing_packet_capacity: usize,
    tokio_handle: Handle,
    /// Store this here so we don't drop it.
    _tokio_runtime: Option<Runtime>,
    dimensions: Vec<Dimension>,
    biomes: Vec<Biome>,
    /// Contains info about dimensions, biomes, and chats.
    /// Sent to all clients when joining.
    registry_codec: Compound,
    /// The instant the server was started.
    start_instant: Instant,
    /// Receiver for new clients past the login stage.
    new_clients_rx: Receiver<NewClientMessage>,
    new_clients_tx: Sender<NewClientMessage>,
    /// Incremented on every game tick.
    tick_counter: AtomicI64,
    /// A semaphore used to limit the number of simultaneous connections to the
    /// server. Closing this semaphore stops new connections.
    connection_sema: Arc<Semaphore>,
    /// The result that will be returned when the server is shut down.
    shutdown_result: Mutex<Option<ShutdownResult>>,
    /// The RSA keypair used for encryption with clients.
    rsa_key: RsaPrivateKey,
    /// The public part of `rsa_key` encoded in DER, which is an ASN.1 format.
    /// This is sent to clients during the authentication process.
    public_key_der: Box<[u8]>,
    /// For session server requests.
    http_client: HttpClient,
}

/// Contains information about a new client.
pub struct NewClientData {
    /// The UUID of the new client.
    pub uuid: Uuid,
    /// The username of the new client.
    pub username: String,
    /// The new client's player textures. May be `None` if the client does not
    /// have a skin or cape.
    pub textures: Option<SignedPlayerTextures>,
    /// The remote address of the new client.
    pub remote_addr: SocketAddr,
}

struct NewClientMessage {
    ncd: NewClientData,
    reply: oneshot::Sender<S2cPacketChannels>,
}

/// The result type returned from [`start_server`].
pub type ShutdownResult = Result<(), Box<dyn Error + Send + Sync + 'static>>;

pub(crate) type S2cPacketChannels = (Sender<C2sPlayPacket>, Receiver<S2cPlayMessage>);
pub(crate) type C2sPacketChannels = (Sender<S2cPlayMessage>, Receiver<C2sPlayPacket>);

/// Messages sent to packet encoders.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub(crate) enum S2cPlayMessage {
    /// Queue a play packet for sending.
    Queue(S2cPlayPacket),
    /// Instructs the encoder to flush all queued packets to the TCP stream.
    Flush,
}

impl<P: Into<S2cPlayPacket>> From<P> for S2cPlayMessage {
    fn from(pkt: P) -> Self {
        Self::Queue(pkt.into())
    }
}

impl<C: Config> SharedServer<C> {
    /// Gets a reference to the config object used to start the server.
    pub fn config(&self) -> &C {
        &self.0.cfg
    }

    /// Gets the socket address this server is bound to.
    pub fn address(&self) -> SocketAddr {
        self.0.address
    }

    /// Gets the configured tick rate of this server.
    pub fn tick_rate(&self) -> Ticks {
        self.0.tick_rate
    }

    /// Gets the connection mode of the server.
    pub fn connection_mode(&self) -> ConnectionMode {
        self.0.connection_mode.clone()
    }

    /// Gets the maximum number of connections allowed to the server at once.
    pub fn max_connections(&self) -> usize {
        self.0.max_connections
    }

    /// Gets the configured incoming packet capacity.
    pub fn incoming_packet_capacity(&self) -> usize {
        self.0.incoming_packet_capacity
    }

    /// Gets the configured outgoing incoming packet capacity.
    pub fn outgoing_packet_capacity(&self) -> usize {
        self.0.outgoing_packet_capacity
    }

    /// Gets a handle to the tokio instance this server is using.
    pub fn tokio_handle(&self) -> &Handle {
        &self.0.tokio_handle
    }

    /// Obtains a [`Dimension`] by using its corresponding [`DimensionId`].
    ///
    /// It is safe but unspecified behavior to call this function using a
    /// [`DimensionId`] not originating from the configuration used to construct
    /// the server.
    pub fn dimension(&self, id: DimensionId) -> &Dimension {
        self.0
            .dimensions
            .get(id.0 as usize)
            .expect("invalid dimension ID")
    }

    /// Returns an iterator over all added dimensions and their associated
    /// [`DimensionId`].
    pub fn dimensions(&self) -> impl FusedIterator<Item = (DimensionId, &Dimension)> + Clone {
        self.0
            .dimensions
            .iter()
            .enumerate()
            .map(|(i, d)| (DimensionId(i as u16), d))
    }

    /// Obtains a [`Biome`] by using its corresponding [`BiomeId`].
    ///
    /// It is safe but unspecified behavior to call this function using a
    /// [`BiomeId`] not originating from the configuration used to construct
    /// the server.
    pub fn biome(&self, id: BiomeId) -> &Biome {
        self.0.biomes.get(id.0 as usize).expect("invalid biome ID")
    }

    /// Returns an iterator over all added biomes and their associated
    /// [`BiomeId`] in ascending order.
    pub fn biomes(
        &self,
    ) -> impl ExactSizeIterator<Item = (BiomeId, &Biome)> + DoubleEndedIterator + FusedIterator + Clone
    {
        self.0
            .biomes
            .iter()
            .enumerate()
            .map(|(i, b)| (BiomeId(i as u16), b))
    }

    pub(crate) fn registry_codec(&self) -> &Compound {
        &self.0.registry_codec
    }

    /// Returns the instant the server was started.
    pub fn start_instant(&self) -> Instant {
        self.0.start_instant
    }

    /// Returns the number of ticks that have elapsed since the server began.
    pub fn current_tick(&self) -> Ticks {
        self.0.tick_counter.load(Ordering::SeqCst)
    }

    /// Immediately stops new connections to the server and initiates server
    /// shutdown. The given result is returned through [`start_server`].
    ///
    /// You may want to disconnect all players with a message prior to calling
    /// this function.
    pub fn shutdown<R, E>(&self, res: R)
    where
        R: Into<Result<(), E>>,
        E: Into<Box<dyn Error + Send + Sync + 'static>>,
    {
        self.0.connection_sema.close();
        *self.0.shutdown_result.lock().unwrap() = Some(res.into().map_err(|e| e.into()));
    }
}

/// Consumes the configuration and starts the server.
///
/// The function returns once the server has shut down, a runtime error
/// occurs, or the configuration is found to be invalid.
pub fn start_server<C: Config>(config: C, data: C::ServerState) -> ShutdownResult {
    let shared = setup_server(config)
        .context("failed to initialize server")
        .map_err(Box::<dyn Error + Send + Sync + 'static>::from)?;

    let _guard = shared.tokio_handle().enter();

    let mut server = Server {
        state: data,
        shared: shared.clone(),
        clients: Clients::new(),
        entities: Entities::new(),
        worlds: Worlds::new(shared.clone()),
        player_lists: PlayerLists::new(),
    };

    shared.config().init(&mut server);

    tokio::spawn(do_accept_loop(shared));

    do_update_loop(&mut server)
}

fn setup_server<C: Config>(cfg: C) -> anyhow::Result<SharedServer<C>> {
    let max_connections = cfg.max_connections();
    let address = cfg.address();
    let tick_rate = cfg.tick_rate();

    ensure!(tick_rate > 0, "tick rate must be greater than zero");

    let connection_mode = cfg.connection_mode();

    let incoming_packet_capacity = cfg.incoming_packet_capacity();

    ensure!(
        incoming_packet_capacity > 0,
        "serverbound packet capacity must be nonzero"
    );

    let outgoing_packet_capacity = cfg.outgoing_packet_capacity();

    ensure!(
        outgoing_packet_capacity > 0,
        "outgoing packet capacity must be nonzero"
    );

    let tokio_handle = cfg.tokio_handle();

    let dimensions = cfg.dimensions();
    validate_dimensions(&dimensions)?;

    let biomes = cfg.biomes();
    validate_biomes(&biomes)?;

    let rsa_key = RsaPrivateKey::new(&mut OsRng, 1024)?;

    let public_key_der =
        rsa_der::public_key_to_der(&rsa_key.n().to_bytes_be(), &rsa_key.e().to_bytes_be())
            .into_boxed_slice();

    let (new_clients_tx, new_clients_rx) = flume::bounded(1);

    let runtime = if tokio_handle.is_none() {
        Some(Runtime::new()?)
    } else {
        None
    };

    let tokio_handle = match &runtime {
        Some(rt) => rt.handle().clone(),
        None => tokio_handle.unwrap(),
    };

    let registry_codec = make_registry_codec(&dimensions, &biomes);

    let server = SharedServerInner {
        cfg,
        address,
        tick_rate,
        connection_mode,
        max_connections,
        incoming_packet_capacity,
        outgoing_packet_capacity,
        tokio_handle,
        _tokio_runtime: runtime,
        dimensions,
        biomes,
        registry_codec,
        start_instant: Instant::now(),
        new_clients_rx,
        new_clients_tx,
        tick_counter: AtomicI64::new(0),
        connection_sema: Arc::new(Semaphore::new(max_connections)),
        shutdown_result: Mutex::new(None),
        rsa_key,
        public_key_der,
        http_client: HttpClient::new(),
    };

    Ok(SharedServer(Arc::new(server)))
}

fn make_registry_codec(dimensions: &[Dimension], biomes: &[Biome]) -> Compound {
    compound! {
        ident!("dimension_type") => compound! {
            "type" => ident!("dimension_type"),
            "value" => List::Compound(dimensions.iter().enumerate().map(|(id, dim)| compound! {
                "name" => DimensionId(id as u16).dimension_type_name(),
                "id" => id as i32,
                "element" => dim.to_dimension_registry_item(),
            }).collect()),
        },
        ident!("worldgen/biome") => compound! {
            "type" => ident!("worldgen/biome"),
            "value" => {
                let mut biomes: Vec<_> = biomes
                    .iter()
                    .enumerate()
                    .map(|(id, biome)| biome.to_biome_registry_item(id as i32))
                    .collect();

                // The client needs a biome named "minecraft:plains" in the registry to
                // connect. This is probably a bug in the client.
                //
                // If the issue is resolved, remove this if.
                if !biomes.iter().any(|b| b["name"] == "plains".into()) {
                    let biome = Biome::default();
                    assert_eq!(biome.name, ident!("plains"));
                    biomes.push(biome.to_biome_registry_item(biomes.len() as i32));
                }

                List::Compound(biomes)
            }
        },
        ident!("chat_type_registry") => compound! {
            "type" => ident!("chat_type"),
            "value" => List::Compound(Vec::new()),
        },
    }
}

fn do_update_loop<C: Config>(server: &mut Server<C>) -> ShutdownResult {
    let mut tick_start = Instant::now();

    let shared = server.shared.clone();
    loop {
        if let Some(res) = shared.0.shutdown_result.lock().unwrap().take() {
            return res;
        }

        while let Ok(msg) = shared.0.new_clients_rx.try_recv() {
            join_player(server, msg);
        }

        // Get serverbound packets first so they are not dealt with a tick late.
        server.clients.par_iter_mut().for_each(|(_, client)| {
            client.handle_serverbound_packets(&server.entities);
        });

        shared.config().update(server);

        server.worlds.par_iter_mut().for_each(|(id, world)| {
            // Chunks created this tick can have their changes applied immediately because
            // they have not been observed by clients yet. Clients will not have to be sent
            // the block change packet in this case, since the changes are applied before we
            // update clients.
            world.chunks.update_created_this_tick();

            world.spatial_index.update(&server.entities, id);
        });

        server.clients.par_iter_mut().for_each(|(_, client)| {
            client.update(
                &shared,
                &server.entities,
                &server.worlds,
                &server.player_lists,
            );
        });

        server.entities.update();

        server.worlds.par_iter_mut().for_each(|(_, world)| {
            world.chunks.update();
        });

        server.player_lists.update();

        // Sleep for the remainder of the tick.
        let tick_duration = Duration::from_secs_f64((shared.0.tick_rate as f64).recip());
        thread::sleep(tick_duration.saturating_sub(tick_start.elapsed()));

        tick_start = Instant::now();
        shared.0.tick_counter.fetch_add(1, Ordering::SeqCst);
    }
}

fn join_player<C: Config>(server: &mut Server<C>, msg: NewClientMessage) {
    let (clientbound_tx, clientbound_rx) = flume::bounded(server.shared.0.outgoing_packet_capacity);
    let (serverbound_tx, serverbound_rx) = flume::bounded(server.shared.0.incoming_packet_capacity);

    let s2c_packet_channels: S2cPacketChannels = (serverbound_tx, clientbound_rx);
    let c2s_packet_channels: C2sPacketChannels = (clientbound_tx, serverbound_rx);

    let _ = msg.reply.send(s2c_packet_channels);

    let client = Client::new(c2s_packet_channels, msg.ncd, C::ClientState::default());

    server.clients.insert(client);
}

struct Codec {
    enc: Encoder<OwnedWriteHalf>,
    dec: Decoder<OwnedReadHalf>,
}

async fn do_accept_loop<C: Config>(server: SharedServer<C>) {
    log::trace!("entering accept loop");

    let listener = match TcpListener::bind(server.0.address).await {
        Ok(listener) => listener,
        Err(e) => {
            server.shutdown(Err(e).context("failed to start TCP listener"));
            return;
        }
    };

    loop {
        match server.0.connection_sema.clone().acquire_owned().await {
            Ok(permit) => match listener.accept().await {
                Ok((stream, remote_addr)) => {
                    let server = server.clone();
                    tokio::spawn(async move {
                        if let Err(e) = stream.set_nodelay(true) {
                            log::error!("failed to set TCP_NODELAY: {e}");
                        }

                        if let Err(e) = handle_connection(server, stream, remote_addr).await {
                            if let Some(e) = e.downcast_ref::<io::Error>() {
                                if e.kind() == io::ErrorKind::UnexpectedEof {
                                    return;
                                }
                            }
                            log::error!("connection to {remote_addr} ended: {e:#}");
                        }
                        drop(permit);
                    });
                }
                Err(e) => {
                    log::error!("failed to accept incoming connection: {e}");
                }
            },
            // Closed semaphore indicates server shutdown.
            Err(_) => return,
        }
    }
}

async fn handle_connection<C: Config>(
    server: SharedServer<C>,
    stream: TcpStream,
    remote_addr: SocketAddr,
) -> anyhow::Result<()> {
    let timeout = Duration::from_secs(10);

    let (read, write) = stream.into_split();
    let mut c = Codec {
        enc: Encoder::new(write, timeout),
        dec: Decoder::new(read, timeout),
    };

    // TODO: peek stream for 0xFE legacy ping

    let handshake: Handshake = c.dec.read_packet::<Handshake>().await?;
    ensure!(
        handshake.server_address.chars().count() <= 255
            || server.connection_mode() == ConnectionMode::Bungeecord,
        "server address too long"
    );

    match handshake.next_state {
        HandshakeNextState::Status => handle_status(server, &mut c, remote_addr, handshake)
            .await
            .context("error during status"),
        HandshakeNextState::Login => match handle_login(&server, &mut c, remote_addr, handshake)
            .await
            .context("error during login")?
        {
            Some(npd) => handle_play(&server, c, npd)
                .await
                .context("error during play"),
            None => Ok(()),
        },
    }
}

async fn handle_status<C: Config>(
    server: SharedServer<C>,
    c: &mut Codec,
    remote_addr: SocketAddr,
    handshake: Handshake,
) -> anyhow::Result<()> {
    c.dec.read_packet::<StatusRequest>().await?;

    match server
        .0
        .cfg
        .server_list_ping(&server, remote_addr, handshake.protocol_version.0)
        .await
    {
        ServerListPing::Respond {
            online_players,
            max_players,
            player_sample,
            description,
            favicon_png,
        } => {
            let mut json = json!({
                "version": {
                    "name": VERSION_NAME,
                    "protocol": PROTOCOL_VERSION
                },
                "players": {
                    "online": online_players,
                    "max": max_players,
                    "sample": player_sample,
                },
                "description": description,
            });

            if let Some(data) = favicon_png {
                let mut buf = "data:image/png;base64,".to_owned();
                base64::encode_config_buf(data, base64::STANDARD, &mut buf);
                json.as_object_mut()
                    .unwrap()
                    .insert("favicon".to_owned(), Value::String(buf));
            }

            c.enc
                .write_packet(&StatusResponse {
                    json_response: json.to_string(),
                })
                .await?;
        }
        ServerListPing::Ignore => return Ok(()),
    }

    let PingRequest { payload } = c.dec.read_packet().await?;

    c.enc.write_packet(&PingResponse { payload }).await?;

    Ok(())
}

/// Handle the login process and return the new player's data if successful.
async fn handle_login<C: Config>(
    server: &SharedServer<C>,
    c: &mut Codec,
    remote_addr: SocketAddr,
    handshake: Handshake,
) -> anyhow::Result<Option<NewClientData>> {
    let mut client_addr = remote_addr;
    if handshake.protocol_version.0 != PROTOCOL_VERSION {
        // TODO: send translated disconnect msg?
        return Ok(None);
    }

    let LoginStart {
        username: BoundedString(username),
        sig_data: _,   // TODO
        profile_id: _, // TODO
    } = c.dec.read_packet().await?;

    ensure!(valid_username(&username), "invalid username '{username}'");

    let (uuid, textures) = match server.connection_mode() {
        ConnectionMode::Online => {
            let my_verify_token: [u8; 16] = rand::random();

            c.enc
                .write_packet(&EncryptionRequest {
                    server_id: Default::default(), // Always empty
                    public_key: server.0.public_key_der.to_vec(),
                    verify_token: my_verify_token.to_vec().into(),
                })
                .await?;

            let EncryptionResponse {
                shared_secret: BoundedArray(encrypted_shared_secret),
                token_or_sig,
            } = c.dec.read_packet().await?;

            let shared_secret = server
                .0
                .rsa_key
                .decrypt(PaddingScheme::PKCS1v15Encrypt, &encrypted_shared_secret)
                .context("failed to decrypt shared secret")?;

            let _opt_signature = match token_or_sig {
                VerifyTokenOrMsgSig::VerifyToken(BoundedArray(encrypted_verify_token)) => {
                    let verify_token = server
                        .0
                        .rsa_key
                        .decrypt(PaddingScheme::PKCS1v15Encrypt, &encrypted_verify_token)
                        .context("failed to decrypt verify token")?;

                    ensure!(
                        my_verify_token.as_slice() == verify_token,
                        "verify tokens do not match"
                    );
                    None
                }
                VerifyTokenOrMsgSig::MsgSig(sig) => Some(sig),
            };

            let crypt_key: [u8; 16] = shared_secret
                .as_slice()
                .try_into()
                .context("shared secret has the wrong length")?;

            c.enc.enable_encryption(&crypt_key);
            c.dec.enable_encryption(&crypt_key);

            #[derive(Debug, Deserialize)]
            struct AuthResponse {
                id: String,
                name: String,
                properties: Vec<Property>,
            }

            let hash = Sha1::new()
                .chain(&shared_secret)
                .chain(&server.0.public_key_der)
                .finalize();

            let hex_hash = auth_digest(&hash);

            let url = C::format_session_server_url(
                server.config(),
                server,
                &username,
                &hex_hash,
                &remote_addr.ip(),
            );
            let resp = server.0.http_client.get(url).send().await?;

            match resp.status() {
                StatusCode::OK => {}
                StatusCode::NO_CONTENT => {
                    let reason = Text::translate("multiplayer.disconnect.unverified_username");
                    c.enc.write_packet(&DisconnectLogin { reason }).await?;
                    bail!("Could not verify username");
                }
                status => {
                    bail!("session server GET request failed (status code {status})");
                }
            }

            let data: AuthResponse = resp.json().await?;

            ensure!(data.name == username, "usernames do not match");

            let uuid = Uuid::parse_str(&data.id).context("failed to parse player's UUID")?;

            let textures = match data.properties.into_iter().find(|p| p.name == "textures") {
                Some(p) => SignedPlayerTextures::from_base64(
                    p.value,
                    p.signature.context("missing signature for textures")?,
                )?,
                None => bail!("failed to find textures in auth response"),
            };

            (uuid, Some(textures))
        }
        ConnectionMode::Offline => {
            // Derive the player's UUID from a hash of their username.
            let uuid = Uuid::from_slice(&Sha256::digest(&username)[..16]).unwrap();

            (uuid, None)
        }
        ConnectionMode::Bungeecord => {
            let mut skin: Option<SignedPlayerTextures> = None;

            // Get data from server_address field of the handshake
            let data = handshake.server_address.split('\0').collect::<Vec<&str>>();
            ensure!(data.len() == 4, "bungeecord data invalid");

            // Get player uuid
            let uuid = Uuid::from_str(data[2]).unwrap();

            // read properties and get skin
            let properties: Vec<Value> = serde_json::from_str(data[3]).unwrap();
            for property_option in properties.iter().map(|p| p.as_object()) {
                match property_option {
                    Some(property) => {
                        let name = property.get("name").unwrap().as_str().unwrap();
                        if name != "textures" {
                            continue;
                        }
                        let value = property.get("value").unwrap().as_str().unwrap();
                        let empty_string_value: Value = Value::String(String::new());
                        let signature = property
                            .get("signature")
                            .get_or_insert(&empty_string_value)
                            .as_str()
                            .unwrap();
                        skin = Some(
                            SignedPlayerTextures::from_base64(
                                String::from(value),
                                String::from(signature),
                            )
                            .unwrap(),
                        );
                    }
                    None => {}
                }
            }
            (uuid, skin)
        }
        ConnectionMode::Velocity { secret } => {
            let message_id: i32 = i32::MIN + 1;
            // Send Player Info Request into the Plugin Channel
            c.enc
                .write_packet(&LoginPluginRequest {
                    message_id: VarInt::from(message_id),
                    channel: Ident::new(VELOCITY_PLAYER_INFO_CHANNEL).unwrap(),
                    data: RawBytes(
                        vec![VELOCITY_SUPPORTED_VERSION as u8],
                    ),
                })
                .await?;

            // Get Response
            let plugin_response: LoginPluginResponse = c.dec.read_packet().await?;

            ensure!(
                plugin_response.message_id.0 == message_id,
                "plugin messages ids do not match"
            );
            let raw_data = plugin_response.data.expect("could not read data");
            let mut data = raw_data.0.as_slice();
            let mut signature: [u8; 32] = [0u8; 32];
            ensure!(
                data.read(&mut signature).unwrap() == 32,
                "can not read signature"
            );

            // Verify Signature
            let all_data_without_signature: &[u8] = &raw_data.0.as_slice()[32..raw_data.0.len()];
            type HmacSha256 = Hmac<Sha256>;
            let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
            Mac::update(&mut mac, all_data_without_signature);
            let signature_check = mac.verify_slice(signature.as_slice());
            if signature_check.is_err() {
                c.enc
                    .write_packet(&DisconnectLogin {
                        reason: Text::text("This server requires you to connect with velocity!"),
                    })
                    .await?;
                log::warn!(
                    "Client tried connect with invalid signature. This could be an attacker or a \
                     miss configured velocity secret key!"
                );
                return Ok(None);
            }

            // Check Velocity Version
            let version: VarInt = Decode::decode(&mut data).unwrap();
            if version.0 > VELOCITY_MAX_SUPPORTED_VERSION {
                c.enc
                    .write_packet(&DisconnectLogin {
                        reason: Text::text("Velocity Version not supported!"),
                    })
                    .await?;
                log::warn!("Velocity Version {} is not supported", version.0);
                return Ok(None);
            }

            // Overwrite Client Address
            let connection_address: String = String::decode(&mut data).unwrap() + ":0";
            client_addr = SocketAddr::from_str(connection_address.as_str()).unwrap();

            // Get UUID and validate username
            let uuid = Uuid::from_u128(data.read_u128::<BigEndian>().unwrap()); // TODO
            let velocity_username: BoundedString<0, 16> = Decode::decode(&mut data).unwrap();
            ensure!(username == velocity_username.0, "usernames do not match");

            // Read Properties and get Skin
            let num_properties = VarInt::decode(&mut data).unwrap();
            let mut skin: Option<SignedPlayerTextures> = None;

            for _ in 0..num_properties.0 {
                let name: String = Decode::decode(&mut data).unwrap();
                let value: String = Decode::decode(&mut data).unwrap();
                let has_signature: bool = Decode::decode(&mut data).unwrap();
                let property_signature: String = if has_signature {
                    Decode::decode(&mut data).unwrap()
                } else {
                    String::new()
                };
                if name == *"textures" {
                    skin =
                        Some(SignedPlayerTextures::from_base64(value, property_signature).unwrap());
                }
            }

            // TODO: implement VELOCITY_MODERN_FORWARDING_WITH_KEY_V2

            (uuid, skin)
        }
    };

    let compression_threshold = 256;
    c.enc
        .write_packet(&SetCompression {
            threshold: VarInt(compression_threshold as i32),
        })
        .await?;

    c.enc.enable_compression(compression_threshold);
    c.dec.enable_compression(compression_threshold);

    let ncd = NewClientData {
        uuid,
        username,
        textures,
        remote_addr: client_addr,
    };

    if let Err(reason) = server.0.cfg.login(server, &ncd).await {
        log::info!("Disconnect at login: \"{reason}\"");
        c.enc.write_packet(&DisconnectLogin { reason }).await?;
        return Ok(None);
    }

    c.enc
        .write_packet(&LoginSuccess {
            uuid: ncd.uuid,
            username: ncd.username.clone().into(),
            properties: Vec::new(),
        })
        .await?;

    Ok(Some(ncd))
}

async fn handle_play<C: Config>(
    server: &SharedServer<C>,
    c: Codec,
    ncd: NewClientData,
) -> anyhow::Result<()> {
    let (reply_tx, reply_rx) = oneshot::channel();

    server
        .0
        .new_clients_tx
        .send_async(NewClientMessage {
            ncd,
            reply: reply_tx,
        })
        .await?;

    let (packet_tx, packet_rx) = match reply_rx.await {
        Ok(res) => res,
        Err(_) => return Ok(()), // Server closed
    };

    let Codec { mut enc, mut dec } = c;

    tokio::spawn(async move {
        while let Ok(msg) = packet_rx.recv_async().await {
            match msg {
                S2cPlayMessage::Queue(pkt) => {
                    if let Err(e) = enc.queue_packet(&pkt) {
                        log::debug!("error while queueing play packet: {e:#}");
                        break;
                    }
                }
                S2cPlayMessage::Flush => {
                    if let Err(e) = enc.flush().await {
                        log::debug!("error while flushing packet queue: {e:#}");
                        break;
                    }
                }
            }
        }
    });

    loop {
        let pkt = dec.read_packet().await?;
        if packet_tx.send_async(pkt).await.is_err() {
            break;
        }
    }

    Ok(())
}

fn auth_digest(bytes: &[u8]) -> String {
    BigInt::from_signed_bytes_be(bytes).to_str_radix(16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_digest_correct() {
        assert_eq!(
            auth_digest(&Sha1::digest("Notch")),
            "4ed1f46bbe04bc756bcb17c0c7ce3e4632f06a48"
        );
        assert_eq!(
            auth_digest(&Sha1::digest("jeb_")),
            "-7c9d5b0044c130109a5d7b5fb5c317c02b4e28c1"
        );
        assert_eq!(
            auth_digest(&Sha1::digest("simon")),
            "88e16a1019277b15d58faf0541e11910eb756f6"
        );
    }
}
