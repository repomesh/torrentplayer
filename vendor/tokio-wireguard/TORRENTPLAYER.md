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

Handshake rate limiter reset (`src/interface/tunnel.rs`, `Tunnel::update_timers`, marked
"Modified for TorrentPlayer"): the boringtun `RateLimiter` shared by the peers counts handshake
messages until its owner calls `reset_count`, which boringtun's own device does every second and
`Tunn` does only for a limiter it creates itself. The interface now resets it on every timer
tick (boringtun applies it at most once a second). Before, the 65th handshake of an interface's
lifetime and every later one was treated as a flood: a handshake response without a cookie was
refused and answered with a cookie reply, so each rekey stalled for at least `REKEY_TIMEOUT`
(or failed with a server that ignores cookies). torrentplayer-net's
`handshakes_keep_completing_after_many_rekeys` test rekeys 80 times and requires the next
handshake to complete at once.
