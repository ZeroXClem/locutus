//! A in-memory connection manager and transport implementation. Used for testing pourpouses.
use std::{io::Cursor, sync::Arc, time::Duration};

use crossbeam::channel::{self, Receiver, Sender};
use once_cell::sync::OnceCell;
use parking_lot::Mutex;

use super::{ConnError, Transport};
use crate::{
    config::tracing::Logger,
    conn_manager::{ConnectionBridge, PeerKey, PeerKeyLocation},
    message::Message,
    ring::Location,
};
static NETWORK_WIRES: OnceCell<(Sender<MessageOnTransit>, Receiver<MessageOnTransit>)> =
    OnceCell::new();

#[derive(Clone)]
pub(crate) struct MemoryConnManager {
    pub transport: InMemoryTransport,
    msg_queue: Arc<Mutex<Vec<Message>>>,
}

impl MemoryConnManager {
    pub fn new(is_open: bool, peer: PeerKey, location: Option<Location>) -> Self {
        Logger::init_logger();
        let transport = InMemoryTransport::new(is_open, peer, location);
        let msg_queue = Arc::new(Mutex::new(Vec::new()));

        let msg_queue_cp = msg_queue.clone();
        let tr_cp = transport.clone();
        tokio::spawn(async move {
            // evaluate the messages as they arrive
            loop {
                let msg = { tr_cp.msg_stack_queue.lock().pop() };
                if let Some(msg) = msg {
                    let msg_data: Message =
                        bincode::deserialize_from(Cursor::new(msg.data)).unwrap();
                    if let Some(mut queue) = msg_queue_cp.try_lock() {
                        queue.push(msg_data);
                        std::mem::drop(queue);
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });

        Self {
            transport,
            msg_queue,
        }
    }
}

#[async_trait::async_trait]
impl ConnectionBridge for MemoryConnManager {
    async fn recv(&self) -> Result<Message, ConnError> {
        loop {
            if let Some(mut queue) = self.msg_queue.try_lock() {
                if let Some(msg) = queue.pop() {
                    return Ok(msg);
                }
                std::mem::drop(queue);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn send(&self, target: &PeerKeyLocation, msg: Message) -> Result<(), ConnError> {
        let msg = bincode::serialize(&msg)?;
        self.transport.send(
            target.peer,
            target.location.ok_or(ConnError::LocationUnknown)?,
            msg,
        );
        Ok(())
    }

    fn add_connection(&mut self, _peer: PeerKeyLocation, _unsolicited: bool) {}
}

#[derive(Clone, Debug)]
struct MessageOnTransit {
    origin: PeerKey,
    origin_loc: Option<Location>,
    target: PeerKey,
    data: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct InMemoryTransport {
    interface_peer: PeerKey,
    location: Option<Location>,
    is_open: bool,
    /// received messages per each peer awaiting processing
    msg_stack_queue: Arc<Mutex<Vec<MessageOnTransit>>>,
    /// all messages 'traversing' the network at a given time
    network: Sender<MessageOnTransit>,
}

impl InMemoryTransport {
    fn new(is_open: bool, interface_peer: PeerKey, location: Option<Location>) -> Self {
        let msg_stack_queue = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = NETWORK_WIRES.get_or_init(crossbeam::channel::unbounded);

        // store messages incoming from the network in the msg stack
        let rcv_msg_c = msg_stack_queue.clone();
        let network = tx.clone();
        let rx = rx.clone();
        tokio::spawn(async move {
            loop {
                match rx.try_recv() {
                    Ok(msg) if msg.target == interface_peer => {
                        log::debug!(
                            "Inbound message received for peer {} from {}",
                            interface_peer,
                            msg.origin
                        );
                        rcv_msg_c.lock().push(msg);
                    }
                    Err(channel::TryRecvError::Disconnected) => break,
                    Err(channel::TryRecvError::Empty) | Ok(_) => {
                        tokio::time::sleep(Duration::from_millis(10)).await
                    }
                }
            }
            log::error!("Stopped receiving messages in {}", interface_peer);
        });

        Self {
            interface_peer,
            location,
            is_open,
            msg_stack_queue,
            network,
        }
    }

    fn send(&self, peer: PeerKey, location: Location, message: Vec<u8>) {
        let send_res = self.network.try_send(MessageOnTransit {
            origin: self.interface_peer,
            origin_loc: Some(location),
            target: peer,
            data: message,
        });
        match send_res {
            Err(channel::TrySendError::Disconnected(_)) => {
                log::error!("Network shutdown")
            }
            Err(channel::TrySendError::Full(_)) => {
                log::error!("not unbounded capacity!");
                panic!();
            }
            Ok(_) => {}
        }
    }
}

impl Transport for InMemoryTransport {
    fn is_open(&self) -> bool {
        self.is_open
    }

    fn location(&self) -> Option<Location> {
        self.location
    }
}