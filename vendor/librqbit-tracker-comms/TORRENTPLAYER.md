Vendored from librqbit-tracker-comms 9.0.1 (https://github.com/ikatson/rqbit), by Igor Katson,
Apache-2.0. The files are the published crate's, unchanged except as noted here and in the
source.

Local change (`src/tracker_comms_udp.rs`, marked "Modified for TorrentPlayer" and "Added for
TorrentPlayer"; `Cargo.toml`): `UdpTrackerClient::new` no longer binds its UDP socket. The
first request binds it, with the same address (`[::]:0`, dual-stack) and bind device, and
starts the reader. Concurrent first requests wait for that one bind (`tokio::sync::OnceCell`).
Upstream, every librqbit session and every torrentplayer-swarm engine with trackers on opened
the socket at start, even with no UDP tracker to contact, so each newly built test binary made
Windows ask whether to allow it through the firewall. A failed bind used to fail the session
or engine start with "error creating UDP socket"; it now fails that tracker request with the
same error (the announce loop logs it and retries), and the next request binds again. The
unit test `the_socket_is_bound_once_by_the_first_request` covers the first bind, concurrent
first requests, reuse, and a failed bind. `Cargo.toml` adds tokio's `sync` feature and a tokio
dev-dependency for that test.

Drop this copy once upstream opens the socket lazily (or lets the caller choose its address).
