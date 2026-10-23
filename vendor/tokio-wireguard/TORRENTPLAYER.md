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

Ephemeral port allocation (`src/interface/allocations.rs`, `Allocations::acquire`, marked
"Modified for TorrentPlayer"): a bind to port 0 starts at a random ephemeral port and must try
each one once. The loop's "back at the start" check matched on the first attempt, so a bind
whose random start port was taken failed with "not a suitable bind address" instead of moving
on. torrentplayer-net's `a_random_port_that_is_taken_moves_on_to_the_next_free_one` test takes
1024 ports and binds 300 more.

Endpoint roaming and handshake rate limiting (`src/interface/tunnel.rs`, `Tunnel::recv` and
`Tunnel::new`, marked "Added/Modified for TorrentPlayer"): the peer's endpoint moved to the
source of any message `decapsulate` accepted, including cookie replies, which anyone who knows
the server's public key and sees an initiation can forge, and the cookie replies the interface
itself sends under load. Like WireGuard, it now roams only on handshake and data messages, and
not when `decapsulate` answers one with a cookie reply (a message the rate limiter refused, so
not authenticated: a forged handshake response sent under load). A
handshake initiation also passes the shared rate limiter (MAC1, and under load the cookie MAC2)
before `parse_handshake_anon`'s key agreement, as in boringtun's own device; since
`decapsulate` checks it again, the limit is 128 so that initiations still get 64 per second.
torrentplayer-net's `a_forged_cookie_reply_does_not_move_the_tunnel_to_its_sender` test forges
a cookie reply (boringtun is a test-only dependency there for this) and fails without the fix.

Interface loop fairness and bounded queues (`src/interface.rs` and `src/interface/device.rs`,
marked "Added/Modified for TorrentPlayer"): the loop selected a readable UDP socket before the
WireGuard timers and the stack poll, and the device queues were unbounded. While datagrams kept
arriving (a busy peer, or a flood of junk), no keepalive, handshake, or rekey went out, so a
session could expire and the tunnel be reported as not handshaking, and received packets could
pile up without limit. Timers now come before received datagrams, at most 64 datagrams are
received in a row before the stack polls, received packets wait in a queue of at most 256, and
each device queue holds at most 1024 packets (more are dropped, as a full interface would).
torrentplayer-net's `a_flood_of_incoming_datagrams_does_not_stall_the_tunnels_timers` test
floods the client's port for 4 seconds and requires its 1-second keepalives to continue; none
were sent before.

TCP timeout and keep-alive options (`src/interface.rs` `TcpOptions::timeout` and
`TcpOptions::keep_alive`, applied in `src/tcp.rs`, marked "Added/Modified for TorrentPlayer"):
smoltcp sockets had neither, so a connection whose peer vanished stayed open for good.
torrentplayer-net sets a 2-minute timeout, 30-second keep-alives, and a listener backlog of 16
(each pending slot holds full buffers).

Socket address option (`src/interface.rs` `Options::bind_address`, used by
`src/interface/tunnel.rs` `Tunnel::new`, marked "Added/Modified for TorrentPlayer"): the
WireGuard UDP socket always bound `[::]` (every interface). `bind_address` keeps that default
and lets torrentplayer-net's tests bind loopback instead (an IPv4 address in its IPv4-mapped
form on the dual-stack socket), so their newly built binaries never trigger a firewall prompt.
