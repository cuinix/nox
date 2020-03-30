/*
 * Copyright 2020 Fluence Labs Limited
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use crate::connect_protocol::behaviour::ClientConnectProtocolBehaviour;
use crate::connect_protocol::messages::ToPeerNetworkMsg;
use crate::relay_api::RelayApi;
use janus_server::{event_polling, generate_swarm_event_type};
use libp2p::identify::{Identify, IdentifyEvent};
use libp2p::identity::PublicKey;
use libp2p::ping::{handler::PingConfig, Ping, PingEvent};
use libp2p::swarm::{NetworkBehaviourAction, NetworkBehaviourEventProcess};
use libp2p::{NetworkBehaviour, PeerId};
use log::debug;
use multihash::Multihash;
use std::collections::VecDeque;

type SwarmEventType = generate_swarm_event_type!(ClientServiceBehaviour);

#[derive(NetworkBehaviour)]
#[behaviour(poll_method = "custom_poll", out_event = "ToPeerNetworkMsg")]
pub struct ClientServiceBehaviour {
    ping: Ping,
    identity: Identify,
    node_connect_protocol: ClientConnectProtocolBehaviour,

    #[behaviour(ignore)]
    events: VecDeque<SwarmEventType>,
}

impl NetworkBehaviourEventProcess<ToPeerNetworkMsg> for ClientServiceBehaviour {
    fn inject_event(&mut self, event: ToPeerNetworkMsg) {
        self.events
            .push_back(NetworkBehaviourAction::GenerateEvent(event));
    }
}

impl NetworkBehaviourEventProcess<PingEvent> for ClientServiceBehaviour {
    fn inject_event(&mut self, event: PingEvent) {
        if event.result.is_err() {
            debug!("ping failed with {:?}", event);
        }
    }
}

impl NetworkBehaviourEventProcess<IdentifyEvent> for ClientServiceBehaviour {
    fn inject_event(&mut self, _event: IdentifyEvent) {}
}

impl ClientServiceBehaviour {
    pub fn new(_local_peer_id: &PeerId, local_public_key: PublicKey) -> Self {
        let ping = Ping::new(
            PingConfig::new()
                .with_max_failures(unsafe { core::num::NonZeroU32::new_unchecked(5) })
                .with_keep_alive(true),
        );
        let identity = Identify::new("1.0.0".into(), "1.0.0".into(), local_public_key);
        let node_connect_protocol = ClientConnectProtocolBehaviour::new();

        Self {
            ping,
            identity,
            node_connect_protocol,
            events: VecDeque::new(),
        }
    }

    #[allow(dead_code)]
    pub fn exit(&mut self) {
        unimplemented!(
            "need to decide how exactly a client should notify the server about disconnecting"
        );
    }

    // produces ToPeerNetworkMsg
    event_polling!(custom_poll, events, SwarmEventType);
}

impl RelayApi for ClientServiceBehaviour {
    fn relay_message(&mut self, relay: PeerId, dst: PeerId, message: Vec<u8>) {
        self.node_connect_protocol
            .relay_message(relay, dst, message);
    }

    fn provide(&mut self, relay: PeerId, key: Multihash) {
        self.node_connect_protocol.provide(relay, key);
    }

    fn find_providers(&mut self, relay: PeerId, client_id: PeerId, key: Multihash) {
        self.node_connect_protocol
            .find_providers(relay, client_id, key);
    }
}
