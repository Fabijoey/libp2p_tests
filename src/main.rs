use futures::stream::StreamExt;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::str::FromStr;
use std::time::Duration;
use tokio::{io, io::AsyncBufReadExt, select};
use anyhow::{Result};
use tracing_subscriber::EnvFilter;
use libp2p::{kad, noise, tcp, yamux, PeerId, gossipsub, Multiaddr, identify};
use libp2p::identity::{Keypair, PublicKey};
use libp2p::kad::{GetClosestPeersError, GetClosestPeersOk, PROTOCOL_NAME, QueryResult};
use libp2p::kad::store::MemoryStore;
use libp2p::swarm::{NetworkBehaviour, SwarmEvent};


#[derive(NetworkBehaviour)]
struct MyBehaviour {
    gossipsub: gossipsub::Behaviour,
    kademlia: kad::Behaviour<MemoryStore>,
    identify: identify::Behaviour
}

fn create_kademlia_behavior(local_peer_id: PeerId) -> kad::Behaviour<MemoryStore> {
    // Create a Kademlia behaviour.
    let mut cfg = kad::Config::new(PROTOCOL_NAME);
    cfg.set_query_timeout(Duration::from_secs(5 * 60));
    let store = MemoryStore::new(local_peer_id);
    kad::Behaviour::with_config(local_peer_id, store, cfg)
}

fn create_identify_behavior(local_public_key: PublicKey) -> identify::Behaviour {
    let id_cfg = identify::Config::new(identify::PROTOCOL_NAME.to_string(),local_public_key);
    println!("{:?}",identify::PROTOCOL_NAME.to_string());
    identify::Behaviour::new(id_cfg)
}

fn create_gossipsub_behavior(id_keys: Keypair) -> gossipsub::Behaviour {
    // To content-address message, we can take the hash of message and use it as an ID.
    let message_id_fn = |message: &gossipsub::Message| {
        let mut s = DefaultHasher::new();
        message.data.hash(&mut s);
        gossipsub::MessageId::from(s.finish().to_string())
    };
    // Set a custom gossipsub
    let gossipsub_config = gossipsub::ConfigBuilder::default()
        .heartbeat_interval(Duration::from_secs(10)) // This is set to aid debugging by not cluttering the log space
        .validation_mode(gossipsub::ValidationMode::Strict) // This sets the kind of message validation. The default is Strict (enforce message signing)
        .message_id_fn(message_id_fn) // content-address messages. No two messages of the same content will be propagated.
        .build()
        .map_err(|msg| io::Error::new(io::ErrorKind::Other, msg)).unwrap(); // Temporary hack because `build` does not return a proper `std::error::Error`.

    gossipsub::Behaviour::new(
        gossipsub::MessageAuthenticity::Signed(id_keys),
        gossipsub_config,
    ).unwrap()
}



#[tokio::main]
async fn main() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .try_init();

    let mut swarm = libp2p::SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_dns()?
        .with_behaviour(|key| {
            Ok(MyBehaviour { gossipsub: create_gossipsub_behavior(key.clone()),
                            kademlia: create_kademlia_behavior(key.public().to_peer_id()) ,
                            identify: create_identify_behavior(key.public())})
        })?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(5)))
        .build();

    println!("My peer addr is {:?}",swarm.local_peer_id());

    let topic = gossipsub::IdentTopic::new("test-net");
    // subscribes to our topic
    swarm.behaviour_mut().gossipsub.subscribe(&topic)?;

    let bootnodes: [(Multiaddr,PeerId); 1] = [
        (Multiaddr::from_str("/ip4/172.17.0.1/tcp/33749")?,PeerId::from_str("12D3KooWBk8UF3zfZJb7r8NraocrvEY2vwvFZfCnGWRG7Zkzs8Ji")?),
       //(Multiaddr::from_str("/dnsaddr/bootstrap.libp2p.io")?,PeerId::from_str("QmNnooDu7bfjPFoTZYxMNLWUQJyrVwtbZg5gBMjTezGAJN")?)
    ];
    // Reach out to another node if specified
    for (peer_addr, peer_id) in bootnodes {
        println!("dialing peer {}", peer_addr);
        swarm.dial(peer_addr.clone())?;
        println!("adding initial peer addr to kademli: routing table: {peer_addr}");
        // Add the bootnodes to the local routing table.
        swarm
            .behaviour_mut()
            .kademlia
            .add_address(&peer_id, peer_addr.clone());
        //doesn't seem to be completely necessary
        swarm
            .behaviour_mut()
            .identify
            .push(std::iter::once(peer_id));
    }

    // Order Kademlia to search for peers.
    let to_search: PeerId = Keypair::generate_ed25519().public().into();
    println!("Searching for the closest peers to {:?}", to_search);
    swarm.behaviour_mut().kademlia.get_closest_peers(to_search);


    // Listen on all interfaces and whatever port the OS assigns
    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;

    // Read full lines from stdin
    let mut stdin = io::BufReader::new(io::stdin()).lines();
    println!("Enter messages via STDIN and they will be sent to connected peers using Gossipsub");



    // Kick it off
    loop {
        select! {
            Ok(Some(line)) = stdin.next_line() => {
                if let Err(e) = swarm
                    .behaviour_mut().gossipsub
                    .publish(topic.clone(), line.as_bytes()) {
                    println!("Publish error: {e:?}");
                }
            }
            event = swarm.select_next_some() => match event  {
                SwarmEvent::Behaviour(MyBehaviourEvent::Kademlia(kad::Event::OutboundQueryProgressed {
                result: QueryResult::GetClosestPeers(result),
                ..
            })) => {
                match result {
                    Ok(GetClosestPeersOk { key, peers }) => {
                        if !peers.is_empty() {
                            println!("Query finished with closest peers: {:#?}", peers);
                            for peer in peers {
                                println!("gossipsub adding peer {peer}");
                                swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer);
                            }
                        } else {
                            println!("Query finished with no closest peers.")
                        }
                    }
                    Err(GetClosestPeersError::Timeout { peers, .. }) => {
                        if !peers.is_empty() {
                            println!("Query timed out with closest peers: {:#?}", peers);
                            for peer in peers {
                                println!("gossipsub adding peer {peer}");
                                swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer);
                            }
                        } else {
                            println!("Query timed out with no closest peers.");
                        }
                    }
                };
            }
            SwarmEvent::Behaviour(MyBehaviourEvent::Identify(identify::Event::Received {
                peer_id,
                info:
                    identify::Info {
                        listen_addrs,
                        protocols,
                        ..
                    },
            })) => {
                if protocols
                    .iter()
                    .any(|p| *p == kad::PROTOCOL_NAME)
                {
                    for addr in listen_addrs {
                        println!("received addr {addr} trough identify");
                        swarm.behaviour_mut().kademlia.add_address(&peer_id, addr);
                    }
                } else {
                    println!("something funky happened, investigate it");
                }
            }

            SwarmEvent::Behaviour(MyBehaviourEvent::Gossipsub(gossipsub::Event::Message {
                propagation_source: peer_id,
                message_id: id,
                message,
            })) => println!(
                    "Got message: '{}'with id: {id} from peer: {peer_id}",
                    String::from_utf8_lossy(&message.data),
                ),
            SwarmEvent::NewListenAddr { address, .. } => {
                println!("Local node is listening on {address}");
            }
            _ => println!("{:?}", event),
            }
        }
    }
}