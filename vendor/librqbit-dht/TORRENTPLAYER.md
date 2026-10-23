Vendored from librqbit-dht 9.0.1 (https://github.com/ikatson/rqbit), by Igor Katson, Apache-2.0.
The files are the published crate's, unchanged except as noted here and in the source.

Local change (`src/dht.rs`, marked "Modified for TorrentPlayer" and "Added for TorrentPlayer"): the UDP reader
skips receive errors that concern a single datagram instead of shutting the whole node down.
Upstream, one oversized datagram (WSAEMSGSIZE, 10040) or one ICMP port-unreachable reply
(WSAECONNRESET) on Windows ended the DHT for the rest of the session, which stopped peer
discovery for both librqbit and torrentplayer-swarm. Other receive errors still end the node.

Additional local change (`src/transport.rs`, `src/lib.rs`, `src/dht.rs`): optional `DhtTransport`
lets the application provide leased send/receive and bootstrap resolution. With this transport,
the node never opens a native UDP socket or uses the system resolver. The default transport
is unchanged. TorrentPlayer uses this seam for bound-interface, WireGuard, and SOCKS5 UDP DHT.
Worker exit cancels the node token so a closed UDP association can be restarted instead of
leaving an apparently live node cached by the application.

One query per hop (`src/dht.rs`, `RecursiveRequest::request_one`, marked "Modified for
TorrentPlayer"): upstream sent every `get_peers` and `find_node` query twice in a row, waited for
both replies, and used only the second (the first only marked the node as responsive and
triggered announces). Every hop of every lookup cost two round trips and twice the datagrams,
and a node that answered only the first time contributed nothing. The first reply is now used.
Measured on a public swarm: the first peer of a lookup from a populated routing table arrives in
about half the time (0.27 s instead of 0.51 s).

Single lookups (`src/dht.rs`, `DhtState::get_peers_once`, `RecursiveRequest::request_peers_once`
and `wait_for_roots`, marked "Added for TorrentPlayer"): `get_peers` runs forever and asks again
every 60 seconds, which cost about 1,100 datagrams a minute for every torrent, paused and complete
ones included. `get_peers_once` runs one lookup per address family and ends the stream once no
query is in flight or queued, so torrentplayer-swarm decides when to look up again. It starts
with the 8 closest nodes of the routing table, waits up to a second for that many while the node
bootstraps, and gives up on an address family whose table stays empty. Announcing
(`announce_port`) works as in `get_peers`, with the given port (`implied_port` 0).
`get_peers` is unchanged for librqbit.

Read-only queries (`src/bprotocol.rs`, `serialize_message_as` and the `ro` field of `RawMessage`;
`src/dht.rs`, `DhtState::set_read_only` and `DhtConfig::read_only`; marked "Added/Modified for
TorrentPlayer"): a read-only node (BEP 43) adds `"ro": 1` to its queries, so other nodes keep
it out of their routing tables and do not send it queries it cannot receive. torrentplayer-swarm
turns it on unless it announces a listening port; librqbit's node is unchanged (off). Replies
never carry the flag, and incoming queries are answered as before.

Drop this copy once upstream supports scoped transports and tolerates these errors.
