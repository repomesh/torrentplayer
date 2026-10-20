Vendored from librqbit-dht 9.0.1 (https://github.com/ikatson/rqbit), by Igor Katson, Apache-2.0.
The files are the published crate's, unchanged except as noted here and in the source.

Local change (`src/dht.rs`, marked "Modified for Lumen" and "Added for Lumen"): the UDP reader
skips receive errors that concern a single datagram instead of shutting the whole node down.
Upstream, one oversized datagram (WSAEMSGSIZE, 10040) or one ICMP port-unreachable reply
(WSAECONNRESET) on Windows ended the DHT for the rest of the session, which stopped peer
discovery for both librqbit and lumen-swarm. Other receive errors still end the node.

Drop this copy once upstream tolerates these errors.
