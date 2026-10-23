// SPDX-License-Identifier: MIT OR Apache-2.0

//! A streaming-first BitTorrent engine for TorrentPlayer.
//!
//! General-purpose clients optimize for total throughput. TorrentPlayer needs the next few seconds of
//! video *now*, so this engine schedules requests by deadline, keeps per-peer request queues
//! short (sized to each peer's measured rate), cancels stale requests on a seek, and requests
//! urgent blocks from a second peer when the first is slow. See [`sched`] for the policy and
//! `docs/adr/0006-streaming-engine.md` for the design.
//!
//! Thin swarms get extra care: peers that drop or cannot be reached are retried with backoff
//! (and never dropped for slowness while they are few), more peers are learned through peer
//! exchange (BEP 11), and a torrent's web seeds (BEP 19) are used as HTTP peers, through the
//! network manager and never towards the user's own network.
//!
//! Tracker communication and `.torrent` parsing come from librqbit's standalone crates; the
//! peer wire protocol is a small, strict codec in [`wire`](crate) so the untrusted-input
//! boundary is easy to audit. Everything that decides streaming performance lives here.

#![forbid(unsafe_code)]

mod bencode;
mod dht;
mod dht_transport;
mod limit;
mod metainfo;
mod peer;
mod pex;
mod reader;
pub mod sched;
mod store;

mod torrent;
pub mod udp_tracker;
mod webseed;
mod wire;

pub use dht::bootstrap_peers as dht_bootstrap_peers;
pub use metainfo::{
    BLOCK_LEN, MAX_PIECE_LEN, MAX_WEB_SEEDS, MetaFile, Metainfo, MetainfoError, check_lengths,
};
pub use reader::FileReader;
pub use sched::SchedulerConfig;
pub use torrent::{PlaybackEstimate, Swarm, SwarmConfig, SwarmError, SwarmStats, Torrent};
