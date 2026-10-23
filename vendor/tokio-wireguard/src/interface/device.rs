use std::collections::VecDeque;

use smoltcp::phy::{DeviceCapabilities, Medium, RxToken, TxToken};

#[derive(Debug)]
pub struct Device {
    recv: Token,
    send: Token,
    mtu: usize,
}

#[derive(Debug)]
pub struct Token {
    queue: PacketQueue,
    buffer: Vec<u8>,
}

impl Device {
    pub fn new(config: &crate::config::Interface) -> Self {
        Self {
            recv: Token::new(),
            send: Token::new(),
            mtu: config.mtu.unwrap_or(1420),
        }
    }

    pub fn enqueue_received(&mut self, packet: &[u8]) {
        self.recv.queue.enqueue(packet)
    }

    pub fn dequeue_sent(&mut self) -> Option<&[u8]> {
        if !self.send.queue.is_empty() {
            self.send.queue.dequeue(&mut self.send.buffer);
            Some(&self.send.buffer)
        } else {
            None
        }
    }

    pub fn can_send(&self) -> bool {
        !self.send.queue.is_empty()
    }

    /// Added for TorrentPlayer: received packets the stack has not taken yet.
    pub fn received_backlog(&self) -> usize {
        self.recv.queue.lengths.len()
    }
}

impl Token {
    const fn new() -> Self {
        Self {
            queue: PacketQueue {
                lengths: VecDeque::new(),
                buffers: VecDeque::new(),
            },
            buffer: Vec::new(),
        }
    }
}

impl smoltcp::phy::Device for Device {
    type RxToken<'a> = &'a mut Token;
    type TxToken<'a> = &'a mut Token;

    fn receive(
        &mut self,
        _: smoltcp::time::Instant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let Self { recv, send, .. } = self;
        if !recv.queue.is_empty() {
            Some((recv, send))
        } else {
            None
        }
    }

    fn transmit(&mut self, _: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        let Self { send, .. } = self;
        Some(send)
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut capacities = DeviceCapabilities::default();
        capacities.medium = Medium::Ip;
        capacities.max_transmission_unit = self.mtu;
        capacities.max_burst_size = None;
        capacities
    }
}

impl<'a> RxToken for &'a mut Token {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        self.queue.dequeue(&mut self.buffer);
        f(&mut self.buffer)
    }
}

impl<'a> TxToken for &'a mut Token {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        self.buffer.resize(len, 0);
        let result = f(&mut self.buffer);
        self.queue.enqueue(&self.buffer);
        result
    }
}

#[derive(Debug)]
struct PacketQueue {
    lengths: VecDeque<usize>,
    buffers: VecDeque<u8>,
}

/// Added for TorrentPlayer: packets a queue holds at most (about 1.4 MiB at the default MTU).
/// Beyond it, packets are dropped like a full network interface would; TCP retransmits them.
const QUEUE_PACKETS: usize = 1024;

impl PacketQueue {
    fn enqueue(&mut self, packet: &[u8]) {
        // Modified for TorrentPlayer: bounded (see `QUEUE_PACKETS`).
        if self.lengths.len() >= QUEUE_PACKETS {
            return;
        }
        self.lengths.push_back(packet.len());
        self.buffers.extend(packet);
    }

    fn dequeue(&mut self, buf: &mut Vec<u8>) {
        let len = self.lengths.pop_front().unwrap();
        buf.clear();
        buf.extend(self.buffers.drain(..len))
    }

    fn is_empty(&self) -> bool {
        self.lengths.is_empty()
    }
}
