//
//   This Source Code Form is subject to the terms of the Mozilla Public
//   License, v. 2.0. If a copy of the MPL was not distributed with this
//   file, You can obtain one at http://mozilla.org/MPL/2.0/.
//
//! MQTTServer and internals
//!
//! Server Architecture
//! ===================
//!
//! The server consists of multiple parts:
//!
//! - An [`MQTTServer`], the main part of the whole and the user-visible part
//! - The [`SubscriptionManager`] which maintains subscription state
//!
//! Per [MQTT Spec] in 3.1.2.4 the server has to keep the following state:
//!
//! - Whether a session exists -> [`ClientSession`]
//! - The clients subscriptions -> [`SubscriptionManager`]
//!
//! This implementation utilizes "Method B" for QoS 2 protocol flow, as explained in Figure 4.3 of
//! the [MQTT Spec]. This minimizes data being held in the application.
//!
//! - QoS 1 & 2 messages which have been relayed to the client, but not yet acknowledged
//! - QoS 0 & 1 & 2 messages pending transmission
//! - QoS 2 messages which have been received from the client, but have not been acknowledged
//!
//!
//! [MQTT Spec]: http://docs.oasis-open.org/mqtt/mqtt/v3.1.1/os/mqtt-v3.1.1-os.html

mod message;
mod state;
mod subscriptions;

use std::{sync::Arc, time::Duration};

use dashmap::DashMap;
use mqtt_format::v3::{
    connect_return::MConnectReturnCode,
    packet::{MConnack, MConnect, MDisconnect, MPacket, MPuback, MPublish, MSubscribe},
    qos::MQualityOfService,
    strings::MString,
    will::MLastWill,
};
use tokio::{
    io::{AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf},
    net::{TcpListener, ToSocketAddrs},
    sync::Mutex,
};
use tracing::{debug, error, info, trace};

use crate::{error::MqttError, mqtt_stream::MqttStream, PacketIOError};
use subscriptions::{ClientInformation, SubscriptionManager};

use self::{message::MqttMessage, state::ClientState};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClientId(String);

impl ClientId {
    #[allow(dead_code)]
    pub(crate) fn new(id: String) -> Self {
        ClientId(id)
    }
}

impl<'message> TryFrom<MString<'message>> for ClientId {
    type Error = ClientError;

    fn try_from(ms: MString<'message>) -> Result<Self, Self::Error> {
        Ok(ClientId(ms.to_string()))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("An error occured during the handling of a packet")]
    Packet(#[from] PacketIOError),
}

#[derive(Debug)]
pub struct ClientConnection {
    reader: Mutex<ReadHalf<MqttStream>>,
    writer: Mutex<WriteHalf<MqttStream>>,
}

#[derive(Debug)]
enum ClientSource {
    UnsecuredTcp(TcpListener),
    #[allow(dead_code)]
    Duplex(tokio::sync::mpsc::Receiver<DuplexStream>),
}

impl ClientSource {
    async fn accept(&mut self) -> Result<MqttStream, MqttError> {
        Ok({
            match self {
                ClientSource::UnsecuredTcp(listener) => listener
                    .accept()
                    .await
                    .map(|tpl| tpl.0)
                    .map(MqttStream::UnsecuredTcp)?,
                ClientSource::Duplex(recv) => recv
                    .recv()
                    .await
                    .map(MqttStream::MemoryDuplex)
                    .ok_or(MqttError::DuplexSourceClosed)?,
            }
        })
    }
}

pub struct MqttServer {
    clients: DashMap<ClientId, ClientState>,
    client_source: ClientSource,
    subscription_manager: SubscriptionManager,
}

impl MqttServer {
    pub async fn serve_v3_unsecured_tcp<Addr: ToSocketAddrs>(
        addr: Addr,
    ) -> Result<Self, MqttError> {
        let bind = TcpListener::bind(addr).await?;

        Ok(MqttServer {
            clients: DashMap::new(),
            client_source: ClientSource::UnsecuredTcp(bind),
            subscription_manager: SubscriptionManager::new(),
        })
    }

    pub async fn accept_new_clients(&mut self) -> Result<(), MqttError> {
        loop {
            let client = self.client_source.accept().await?;
            if let Err(client_error) = self.accept_client(client).await {
                tracing::error!("Client error: {}", client_error)
            }
        }
    }

    /// Accept a new client connected through the `client` stream
    ///
    /// This does multiple things:
    ///
    /// - It checks whether a client with that given ID exists
    ///     - If yes, then that session is replaced when clean_session = true
    ///
    async fn accept_client(&self, mut client: MqttStream) -> Result<(), ClientError> {
        async fn send_connack(
            session_present: bool,
            connect_return_code: MConnectReturnCode,
            client: &mut MqttStream,
        ) -> Result<(), ClientError> {
            let conn_ack = MConnack {
                session_present,
                connect_return_code,
            };

            crate::write_packet(client, conn_ack).await?;

            Ok(())
        }

        #[allow(clippy::too_many_arguments)]
        async fn connect_client<'message>(
            server: &MqttServer,
            mut client: MqttStream,
            _protocol_name: MString<'message>,
            _protocol_level: u8,
            clean_session: bool,
            will: Option<MLastWill<'message>>,
            _username: Option<MString<'message>>,
            _password: Option<&'message [u8]>,
            keep_alive: u16,
            client_id: MString<'message>,
        ) -> Result<(), ClientError> {
            let client_id = ClientId::try_from(client_id)?;

            let session_present = if clean_session {
                let _ = server.clients.remove(&client_id);
                false
            } else {
                server.clients.contains_key(&client_id)
            };

            send_connack(session_present, MConnectReturnCode::Accepted, &mut client).await?;

            let (client_reader, client_writer) = tokio::io::split(client);

            let client_connection = Arc::new(ClientConnection {
                reader: Mutex::new(client_reader),
                writer: Mutex::new(client_writer),
            });

            {
                let state = server
                    .clients
                    .entry(client_id.clone())
                    .or_insert_with(ClientState::default);
                state.set_new_connection(client_connection.clone()).await;
            }

            let client_id = Arc::new(client_id);

            let mut last_will: Option<MqttMessage> = will
                .as_ref()
                .map(|will| MqttMessage::from_last_will(will, client_id.clone()));

            let published_packets = server.subscription_manager.clone();
            let (published_packets_send, mut published_packets_rec) =
                tokio::sync::mpsc::unbounded_channel::<MqttMessage>();

            let _send_loop = {
                let publisher_conn = client_connection.clone();
                let publisher_client_id = client_id.clone();
                tokio::spawn(async move {
                    loop {
                        match published_packets_rec.recv().await {
                            Some(packet) => {
                                if packet.author_id() == &*publisher_client_id {
                                    trace!(?packet, "Skipping sending message to onethis");
                                    continue;
                                }

                                let packet = MPublish {
                                    dup: false,
                                    qos: MQualityOfService::AtMostOnce,
                                    retain: packet.retain(),
                                    topic_name: MString {
                                        value: packet.topic(),
                                    },
                                    id: None,
                                    payload: packet.payload(),
                                };

                                let mut writer = publisher_conn.writer.lock().await;
                                crate::write_packet(&mut *writer, packet).await.unwrap();
                                // 1. Check if subscription matches
                                // 2. If Qos == 0 -> Send into writer
                                // 3. If QoS == 1 -> Send into writer && Store Message waiting for Puback
                                // 4. If QoS == 2 -> Send into writer && Store Message waiting for PubRec
                            }
                            None => {
                                debug!(
                                    ?publisher_client_id,
                                    "No more senders, stopping sending cycle"
                                );
                                break;
                            }
                        }
                    }
                })
            };

            let _read_loop = {
                let keep_alive = keep_alive;
                let subscription_manager = server.subscription_manager.clone();

                tokio::spawn(async move {
                    let client_id = client_id;
                    let client_connection = client_connection;
                    let mut reader = client_connection.reader.lock().await;
                    let keep_alive_duration = Duration::from_secs((keep_alive as u64 * 150) / 100);
                    let subscription_manager = subscription_manager;

                    loop {
                        let packet = tokio::select! {
                            packet = crate::read_one_packet(&mut *reader) => {
                                match packet {
                                    Ok(packet) => packet,
                                    Err(e) => {
                                        debug!("Could not read the next client packet: {e}");
                                        break;
                                    }
                                }
                            },
                            _timeout = tokio::time::sleep(keep_alive_duration) => {
                                debug!("Client timed out");
                                break;
                            }
                        };

                        match packet.get_packet() {
                            MPacket::Publish(MPublish {
                                dup: _,
                                qos,
                                retain,
                                topic_name,
                                id,
                                payload,
                            }) => {
                                let message = MqttMessage::new(
                                    client_id.clone(),
                                    payload.to_vec(),
                                    topic_name.to_string(),
                                    *retain,
                                    *qos,
                                );

                                subscription_manager.route_message(message).await;

                                if *qos == MQualityOfService::AtLeastOnce {
                                    let packet = MPuback { id: id.unwrap() };
                                    let mut writer = client_connection.writer.lock().await;
                                    crate::write_packet(&mut *writer, packet).await?;
                                }

                                // tokio::spawn(publish_state_machine -> {
                                //     if qos == 0  {
                                //         -> Send message to other clients on topic
                                //     }
                                //     if qos == 1 {
                                //         -> Send PUBACK back
                                //         -> Send message to other clients on topic with QOS 1
                                //             published_packets.send(message)
                                //     }
                                //     if qos == 2 {
                                //         -> Store Packet Identifier
                                //         -> Send message to other clients on topic with QOS 2
                                //         -> Send PUBREC
                                //         -> Save in MessageStore with latest state = PUBREC
                                //     }
                                // })
                            }
                            MPacket::Pubrel { .. } => {
                                // -> Check if MessageStore contains state PUBREC with packet id
                            }
                            MPacket::Pubrec { .. } => {
                                // -> Discard message
                                // -> Store PUBREC received
                                // -> Send PUBREL
                            }
                            MPacket::Pubcomp { .. } => {
                                // -> Discard PUBREC
                            }
                            MPacket::Disconnect(MDisconnect) => {
                                last_will.take();
                                debug!("Client disconnected gracefully");
                                break;
                            }
                            MPacket::Subscribe(MSubscribe {
                                id: _,
                                subscriptions,
                            }) => {
                                subscription_manager
                                    .subscribe(
                                        Arc::new(ClientInformation {
                                            client_id: client_id.clone(),
                                            client_sender: published_packets_send.clone(),
                                        }),
                                        *subscriptions,
                                    )
                                    .await;
                            }
                            packet => info!("Received packet: {packet:?}"),
                        }
                    }

                    if let Some(will) = last_will {
                        debug!(?will, "Sending out will");
                        let _ = published_packets.route_message(will);
                    }

                    if let Err(e) = client_connection.writer.lock().await.shutdown().await {
                        debug!("Client could not shut down cleanly: {e}");
                    }

                    Ok::<(), ClientError>(())
                })
            };

            Ok(())
        }

        let packet = crate::read_one_packet(&mut client).await?;

        if let MPacket::Connect(MConnect {
            client_id,
            clean_session,
            protocol_name,
            protocol_level,
            will,
            username,
            password,
            keep_alive,
        }) = packet.get_packet()
        {
            connect_client(
                self,
                client,
                *protocol_name,
                *protocol_level,
                *clean_session,
                *will,
                *username,
                *password,
                *keep_alive,
                *client_id,
            )
            .await?;
        } else {
            // Disconnect and don't worry about errors
            if let Err(e) = client.shutdown().await {
                debug!("Client could not shut down cleanly: {e}");
            }
        }

        Ok(())
    }
}