Vendored from tokio-wireguard 0.1.3 (https://github.com/raftario/river), by Raphaël Thériault, MIT OR Apache-2.0.

Local changes upgrade boringtun to 0.7.1 (current crypto dependencies) and add redacted preshared keys, handshake age reporting, initial handshakes, and IPv4 endpoint mapping for dual-stack UDP sockets. The library supplies the existing boringtun and smoltcp integration without OS adapters or administrator privileges.

Additional fixes cover TCP handshake readiness (avoid treating SYN as closed), live socket registration, write wakers, fair polling, immediate polling after socket I/O, and safe Arc-backed wakers instead of transmute. TCP/DNS transfer and preshared-key authentication are exercised by torrentplayer-net integration tests.

Incoming packets and handshake timers take priority over immediate stack polls, so busy TCP
connections cannot starve acknowledgements or concurrent DNS replies. A regression test closes
old connections and exchanges data on sixteen new connections with concurrent tunneled DNS.

Liveness reporting (`src/interface.rs`, `src/interface/tunnel.rs`, marked "Added for
TorrentPlayer"): `Interface::liveness` reports the handshake age, the time since the last
authenticated transport packet (data or keepalive) from the peer, and how long traffic sent to
the peer has gone unanswered. torrentplayer-net uses it to treat a tunnel whose server completes
handshakes but answers no traffic, or answers nothing at all, as not connected.
