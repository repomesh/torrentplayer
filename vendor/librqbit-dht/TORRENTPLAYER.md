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

Drop this copy once upstream supports scoped transports and tolerates these errors.
