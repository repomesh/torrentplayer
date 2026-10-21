// SPDX-License-Identifier: MIT OR Apache-2.0

//! A strict SOCKS5 handshake: the client side (RFC 1928, with RFC 1929 username/password
//! authentication) and the server side used by TorrentStream's loopback bridge.
//!
//! The client supports CONNECT and UDP ASSOCIATE; the loopback server accepts only CONNECT.
//! Every length and version byte is checked, nothing past the
//! handshake is read from the stream, and credentials are wiped from memory after sending.

use std::fmt;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

use subtle::ConstantTimeEq as _;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use zeroize::Zeroizing;

const VERSION: u8 = 5;
const AUTH_VERSION: u8 = 1;
const METHOD_NONE: u8 = 0x00;
const METHOD_PASSWORD: u8 = 0x02;
const METHOD_REFUSED: u8 = 0xFF;
const CMD_CONNECT: u8 = 0x01;
const ATYP_V4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_V6: u8 = 0x04;

/// Where the proxy should connect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// An IP address and port.
    Addr(SocketAddr),
    /// A host name and port. The proxy resolves the name, so no DNS query leaves this machine.
    Domain(String, u16),
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Addr(addr) => addr.fmt(f),
            Self::Domain(host, port) => write!(f, "{host}:{port}"),
        }
    }
}

/// A SOCKS5 reply code (RFC 1928, section 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    /// The connection was made.
    Succeeded,
    /// General SOCKS server failure.
    GeneralFailure,
    /// Connection not allowed by the proxy's rules.
    NotAllowed,
    /// Network unreachable.
    NetworkUnreachable,
    /// Host unreachable.
    HostUnreachable,
    /// Connection refused by the destination.
    ConnectionRefused,
    /// TTL expired (the destination timed out).
    TtlExpired,
    /// Command not supported.
    CommandNotSupported,
    /// Address type not supported.
    AddressTypeNotSupported,
    /// A code RFC 1928 does not define.
    Unassigned(u8),
}

impl Reply {
    fn from_byte(b: u8) -> Self {
        match b {
            0 => Self::Succeeded,
            1 => Self::GeneralFailure,
            2 => Self::NotAllowed,
            3 => Self::NetworkUnreachable,
            4 => Self::HostUnreachable,
            5 => Self::ConnectionRefused,
            6 => Self::TtlExpired,
            7 => Self::CommandNotSupported,
            8 => Self::AddressTypeNotSupported,
            other => Self::Unassigned(other),
        }
    }

    pub(crate) fn to_byte(self) -> u8 {
        match self {
            Self::Succeeded => 0,
            Self::GeneralFailure => 1,
            Self::NotAllowed => 2,
            Self::NetworkUnreachable => 3,
            Self::HostUnreachable => 4,
            Self::ConnectionRefused => 5,
            Self::TtlExpired => 6,
            Self::CommandNotSupported => 7,
            Self::AddressTypeNotSupported => 8,
            Self::Unassigned(b) => b,
        }
    }

    /// Whether the failure is the destination's (unreachable, refused, timed out) rather than
    /// the proxy's.
    #[must_use]
    pub fn blames_target(self) -> bool {
        matches!(
            self,
            Self::HostUnreachable | Self::ConnectionRefused | Self::TtlExpired
        )
    }
}

impl fmt::Display for Reply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Succeeded => f.write_str("succeeded"),
            Self::GeneralFailure => f.write_str("general proxy failure"),
            Self::NotAllowed => f.write_str("connection not allowed by the proxy"),
            Self::NetworkUnreachable => f.write_str("network unreachable"),
            Self::HostUnreachable => f.write_str("host unreachable"),
            Self::ConnectionRefused => f.write_str("connection refused"),
            Self::TtlExpired => f.write_str("timed out"),
            Self::CommandNotSupported => f.write_str("command not supported"),
            Self::AddressTypeNotSupported => f.write_str("address type not supported"),
            Self::Unassigned(b) => write!(f, "unknown reply code {b}"),
        }
    }
}

/// A failed SOCKS5 handshake.
#[derive(Debug, thiserror::Error)]
pub enum Socks5Error {
    /// The stream failed or closed early.
    #[error("{0}")]
    Io(#[from] io::Error),
    /// The other side sent something RFC 1928 or RFC 1929 does not allow.
    #[error("the SOCKS5 protocol was violated: {0}")]
    Protocol(&'static str),
    /// No authentication method both sides support (the proxy may need a username and
    /// password).
    #[error("the proxy refused every offered authentication method")]
    NoAcceptableMethod,
    /// The username or password was rejected.
    #[error("the proxy rejected the username or password")]
    AuthFailed,
    /// A username or password is empty or longer than 255 bytes.
    #[error("proxy usernames and passwords must be 1-255 bytes")]
    InvalidCredentials,
    /// A host name is empty or longer than 255 bytes.
    #[error("the host name cannot be sent through SOCKS5")]
    InvalidTarget,
    /// The proxy answered the request with an error.
    #[error("{0}")]
    Refused(Reply),
    /// The client asked for something other than `CONNECT` (server side).
    #[error("only CONNECT is supported")]
    UnsupportedCommand,
}

/// Performs the client handshake on `stream` (already connected to the proxy), asking it to
/// connect to `target`. Offers username/password authentication only when `credentials` is set.
///
/// On success the stream carries the connection to `target`.
///
/// # Errors
///
/// Returns an error if the stream fails, the proxy violates the protocol, authentication
/// fails, or the proxy cannot connect.
pub async fn connect<S>(
    stream: &mut S,
    target: &Target,
    credentials: Option<(&str, &str)>,
) -> Result<(), Socks5Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let request = encode_request(target)?;
    negotiate(stream, credentials).await?;
    stream.write_all(&request).await?;
    read_reply(stream).await.map(|_| ())
}

/// Requests a UDP relay after authentication; the TCP stream must stay open for its lifetime.
///
/// # Errors
/// Returns handshake, authentication, or relay refusal errors, without a direct fallback.
pub async fn udp_associate<S>(
    stream: &mut S,
    credentials: Option<(&str, &str)>,
) -> Result<Target, Socks5Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    negotiate(stream, credentials).await?;
    stream
        .write_all(&[VERSION, 3, 0, ATYP_V4, 0, 0, 0, 0, 0, 0])
        .await?;
    read_reply(stream).await
}

/// Negotiates the authentication method and authenticates, without making a request. Used as a
/// health check: it proves the proxy is up and accepts the credentials, and contacts no one else.
///
/// # Errors
///
/// As for [`connect`], except that no connection is requested.
pub async fn negotiate<S>(
    stream: &mut S,
    credentials: Option<(&str, &str)>,
) -> Result<(), Socks5Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let greeting: &[u8] = if credentials.is_some() {
        &[VERSION, 2, METHOD_NONE, METHOD_PASSWORD]
    } else {
        &[VERSION, 1, METHOD_NONE]
    };
    stream.write_all(greeting).await?;
    let mut choice = [0u8; 2];
    stream.read_exact(&mut choice).await?;
    if choice[0] != VERSION {
        return Err(Socks5Error::Protocol("wrong version in method selection"));
    }
    match (choice[1], credentials) {
        (METHOD_NONE, _) => {}
        (METHOD_PASSWORD, Some((user, pass))) => authenticate(stream, user, pass).await?,
        (METHOD_REFUSED, _) => return Err(Socks5Error::NoAcceptableMethod),
        _ => {
            return Err(Socks5Error::Protocol(
                "the proxy chose a method that was not offered",
            ));
        }
    }
    Ok(())
}

async fn authenticate<S>(stream: &mut S, user: &str, pass: &str) -> Result<(), Socks5Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (user, pass) = (user.as_bytes(), pass.as_bytes());
    let (Ok(ulen), Ok(plen)) = (u8::try_from(user.len()), u8::try_from(pass.len())) else {
        return Err(Socks5Error::InvalidCredentials);
    };
    if ulen == 0 || plen == 0 {
        return Err(Socks5Error::InvalidCredentials);
    }
    let mut message = Zeroizing::new(Vec::with_capacity(3 + user.len() + pass.len()));
    message.push(AUTH_VERSION);
    message.push(ulen);
    message.extend_from_slice(user);
    message.push(plen);
    message.extend_from_slice(pass);
    stream.write_all(&message).await?;
    let mut status = [0u8; 2];
    stream.read_exact(&mut status).await?;
    // RFC 1929 says the reply version is 1; some servers echo 5. Both are unambiguous.
    if status[0] != AUTH_VERSION && status[0] != VERSION {
        return Err(Socks5Error::Protocol(
            "wrong version in authentication reply",
        ));
    }
    if status[1] != 0 {
        return Err(Socks5Error::AuthFailed);
    }
    Ok(())
}

pub(crate) fn encode_request(target: &Target) -> Result<Vec<u8>, Socks5Error> {
    let mut out = vec![VERSION, CMD_CONNECT, 0];
    let port = match target {
        Target::Addr(SocketAddr::V4(a)) => {
            out.push(ATYP_V4);
            out.extend_from_slice(&a.ip().octets());
            a.port()
        }
        Target::Addr(SocketAddr::V6(a)) => {
            out.push(ATYP_V6);
            out.extend_from_slice(&a.ip().octets());
            a.port()
        }
        Target::Domain(host, port) => {
            let len = u8::try_from(host.len()).map_err(|_| Socks5Error::InvalidTarget)?;
            if len == 0 || host.bytes().any(|b| b.is_ascii_control()) {
                return Err(Socks5Error::InvalidTarget);
            }
            out.push(ATYP_DOMAIN);
            out.push(len);
            out.extend_from_slice(host.as_bytes());
            *port
        }
    };
    out.extend_from_slice(&port.to_be_bytes());
    Ok(out)
}

async fn read_reply<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Target, Socks5Error> {
    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await?;
    if head[0] != VERSION {
        return Err(Socks5Error::Protocol("wrong version in reply"));
    }
    let reply = Reply::from_byte(head[1]);
    if reply != Reply::Succeeded {
        return Err(Socks5Error::Refused(reply));
    }
    if head[2] != 0 {
        return Err(Socks5Error::Protocol("reserved byte is not zero"));
    }
    // CONNECT consumes this address; UDP ASSOCIATE also uses it to locate the relay.
    read_address(stream, head[3]).await
}

/// Reads an address of type `atyp` and a port. Unknown types are a protocol error.
pub(crate) async fn read_address<S: AsyncRead + Unpin>(
    stream: &mut S,
    atyp: u8,
) -> Result<Target, Socks5Error> {
    let target = match atyp {
        ATYP_V4 => {
            let mut b = [0u8; 6];
            stream.read_exact(&mut b).await?;
            let ip = Ipv4Addr::new(b[0], b[1], b[2], b[3]);
            Target::Addr(SocketAddr::new(ip.into(), u16::from_be_bytes([b[4], b[5]])))
        }
        ATYP_V6 => {
            let mut b = [0u8; 18];
            stream.read_exact(&mut b).await?;
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&b[..16]);
            let ip = Ipv6Addr::from(octets);
            Target::Addr(SocketAddr::new(
                ip.into(),
                u16::from_be_bytes([b[16], b[17]]),
            ))
        }
        ATYP_DOMAIN => {
            let len = stream.read_u8().await?;
            if len == 0 {
                return Err(Socks5Error::Protocol("empty host name"));
            }
            let mut b = vec![0u8; usize::from(len) + 2];
            stream.read_exact(&mut b).await?;
            let port = u16::from_be_bytes([b[b.len() - 2], b[b.len() - 1]]);
            b.truncate(usize::from(len));
            let host = String::from_utf8(b)
                .map_err(|_| Socks5Error::Protocol("host name is not UTF-8"))?;
            if host.chars().any(char::is_control) {
                return Err(Socks5Error::Protocol(
                    "host name contains control characters",
                ));
            }
            Target::Domain(host, port)
        }
        _ => return Err(Socks5Error::Protocol("unknown address type")),
    };
    Ok(target)
}

/// Authentication a server requires.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ServerAuth<'a> {
    /// No authentication.
    #[cfg_attr(not(any(test, feature = "test-util")), allow(dead_code))]
    None,
    /// RFC 1929 username and password (the bridge uses [`ServerAuth::AnyUser`]).
    #[cfg_attr(not(any(test, feature = "test-util")), allow(dead_code))]
    Password {
        /// Expected username.
        username: &'a str,
        /// Expected password, compared in constant time.
        password: &'a str,
    },
    /// RFC 1929 with any of several usernames (each selecting a different service) and one
    /// password. [`accept_as`] reports which username was used.
    AnyUser {
        /// Accepted usernames.
        usernames: &'a [&'a str],
        /// Expected password, compared in constant time.
        password: &'a str,
    },
}

/// Performs the server side of the handshake up to the request, returning the requested target.
/// The caller then answers with [`send_reply`].
///
/// On failure an error reply is sent where the protocol allows one.
#[cfg_attr(not(any(test, feature = "test-util")), allow(dead_code))]
pub(crate) async fn accept<S>(stream: &mut S, auth: ServerAuth<'_>) -> Result<Target, Socks5Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    accept_as(stream, auth).await.map(|(target, _)| target)
}

/// Like [`accept`], also returning the index of the username that authenticated with
/// [`ServerAuth::AnyUser`] (0 for the other kinds).
pub(crate) async fn accept_as<S>(
    stream: &mut S,
    auth: ServerAuth<'_>,
) -> Result<(Target, usize), Socks5Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut head = [0u8; 2];
    stream.read_exact(&mut head).await?;
    if head[0] != VERSION {
        return Err(Socks5Error::Protocol("not a SOCKS5 client"));
    }
    if head[1] == 0 {
        return Err(Socks5Error::Protocol("no authentication methods offered"));
    }
    let mut methods = vec![0u8; usize::from(head[1])];
    stream.read_exact(&mut methods).await?;
    let wanted = match auth {
        ServerAuth::None => METHOD_NONE,
        ServerAuth::Password { .. } | ServerAuth::AnyUser { .. } => METHOD_PASSWORD,
    };
    if !methods.contains(&wanted) {
        stream.write_all(&[VERSION, METHOD_REFUSED]).await?;
        return Err(Socks5Error::NoAcceptableMethod);
    }
    stream.write_all(&[VERSION, wanted]).await?;

    let mut user_index = 0;
    let single: [&str; 1];
    let credentials = match auth {
        ServerAuth::None => None,
        ServerAuth::Password { username, password } => {
            single = [username];
            Some((&single[..], password))
        }
        ServerAuth::AnyUser {
            usernames,
            password,
        } => Some((usernames, password)),
    };
    if let Some((usernames, password)) = credentials {
        let version = stream.read_u8().await?;
        if version != AUTH_VERSION {
            return Err(Socks5Error::Protocol("wrong authentication version"));
        }
        let user = Zeroizing::new(read_counted(stream).await?);
        let pass = Zeroizing::new(read_counted(stream).await?);
        // Usernames are not secret; only the password needs a constant-time comparison.
        let found = usernames
            .iter()
            .position(|u| u.as_bytes() == user.as_slice());
        let pass_ok = pass.as_slice().ct_eq(password.as_bytes());
        if let Some(i) = found {
            user_index = i;
        }
        if found.is_none() || !bool::from(pass_ok) {
            stream.write_all(&[AUTH_VERSION, 1]).await?;
            return Err(Socks5Error::AuthFailed);
        }
        stream.write_all(&[AUTH_VERSION, 0]).await?;
    }

    let mut request = [0u8; 4];
    stream.read_exact(&mut request).await?;
    if request[0] != VERSION || request[2] != 0 {
        return Err(Socks5Error::Protocol("malformed request"));
    }
    if request[1] != CMD_CONNECT {
        send_reply(stream, Reply::CommandNotSupported).await?;
        return Err(Socks5Error::UnsupportedCommand);
    }
    if !matches!(request[3], ATYP_V4 | ATYP_V6 | ATYP_DOMAIN) {
        send_reply(stream, Reply::AddressTypeNotSupported).await?;
        return Err(Socks5Error::Protocol("unknown address type"));
    }
    Ok((read_address(stream, request[3]).await?, user_index))
}

/// Reads a length-prefixed field (RFC 1929): one length byte (1-255), then that many bytes.
async fn read_counted<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Vec<u8>, Socks5Error> {
    let len = stream.read_u8().await?;
    if len == 0 {
        return Err(Socks5Error::Protocol("empty authentication field"));
    }
    let mut field = vec![0u8; usize::from(len)];
    stream.read_exact(&mut field).await?;
    Ok(field)
}

/// Sends a reply to a request. The bound address is always reported as `0.0.0.0:0`; clients do
/// not need it for `CONNECT`.
pub(crate) async fn send_reply<S: AsyncWrite + Unpin>(
    stream: &mut S,
    reply: Reply,
) -> io::Result<()> {
    stream
        .write_all(&[VERSION, reply.to_byte(), 0, ATYP_V4, 0, 0, 0, 0, 0, 0])
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    /// Runs the client against a scripted server: `expect` is what the client must send,
    /// `respond` is sent back after each expected chunk.
    async fn scripted(
        target: Target,
        credentials: Option<(&str, &str)>,
        script: Vec<(Vec<u8>, Vec<u8>)>,
    ) -> Result<(), Socks5Error> {
        let (mut client, mut server) = duplex(1024);
        let server_task = tokio::spawn(async move {
            for (expect, respond) in script {
                let mut got = vec![0u8; expect.len()];
                if server.read_exact(&mut got).await.is_err() {
                    return;
                }
                assert_eq!(got, expect);
                if server.write_all(&respond).await.is_err() {
                    return;
                }
            }
        });
        let result = connect(&mut client, &target, credentials).await;
        drop(client);
        server_task.await.unwrap();
        result
    }

    const OK_REPLY_V4: [u8; 10] = [5, 0, 0, 1, 10, 0, 0, 1, 0x1F, 0x90];

    #[tokio::test]
    async fn udp_associate_authenticates_and_reads_the_relay() {
        let (mut client, mut server) = duplex(1024);
        let task = tokio::spawn(async move {
            for (expect, respond) in [
                (vec![5, 2, 0, 2], vec![5, 2]),
                (b"\x01\x05alice\x06secret".to_vec(), vec![1, 0]),
                (vec![5, 3, 0, 1, 0, 0, 0, 0, 0, 0], OK_REPLY_V4.to_vec()),
            ] {
                let mut got = vec![0; expect.len()];
                server.read_exact(&mut got).await.unwrap();
                assert_eq!(got, expect);
                server.write_all(&respond).await.unwrap();
            }
        });
        assert_eq!(
            udp_associate(&mut client, Some(("alice", "secret")))
                .await
                .unwrap(),
            Target::Addr(addr("10.0.0.1:8080"))
        );
        task.await.unwrap();
    }

    #[tokio::test]
    async fn connects_without_auth_to_an_ipv4_address() {
        let result = scripted(
            Target::Addr(addr("192.0.2.7:6881")),
            None,
            vec![
                (vec![5, 1, 0], vec![5, 0]),
                (
                    vec![5, 1, 0, 1, 192, 0, 2, 7, 0x1A, 0xE1],
                    OK_REPLY_V4.to_vec(),
                ),
            ],
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    async fn sends_host_names_for_remote_dns_and_authenticates() {
        let mut request = vec![5, 1, 0, 3, 15];
        request.extend_from_slice(b"tracker.example");
        request.extend_from_slice(&80u16.to_be_bytes());
        // A reply with a domain-typed bound address must be consumed exactly.
        let mut reply = vec![5, 0, 0, 3, 4];
        reply.extend_from_slice(b"prox");
        reply.extend_from_slice(&[0, 80]);
        let result = scripted(
            Target::Domain("tracker.example".into(), 80),
            Some(("alice", "s3cret")),
            vec![
                (vec![5, 2, 0, 2], vec![5, 2]),
                (b"\x01\x05alice\x06s3cret".to_vec(), vec![1, 0]),
                (request, reply),
            ],
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    async fn encodes_ipv6_targets() {
        let target = Target::Addr(addr("[2001:db8::1]:443"));
        let mut request = vec![5, 1, 0, 4];
        request.extend_from_slice(&"2001:db8::1".parse::<Ipv6Addr>().unwrap().octets());
        request.extend_from_slice(&443u16.to_be_bytes());
        let mut reply = vec![5, 0, 0, 4];
        reply.extend_from_slice(&[0; 18]);
        let result = scripted(
            target,
            None,
            vec![(vec![5, 1, 0], vec![5, 0]), (request, reply)],
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    async fn reports_rejected_credentials() {
        let result = scripted(
            Target::Addr(addr("192.0.2.7:1")),
            Some(("alice", "wrong")),
            vec![
                (vec![5, 2, 0, 2], vec![5, 2]),
                (b"\x01\x05alice\x05wrong".to_vec(), vec![1, 1]),
            ],
        )
        .await;
        assert!(matches!(result, Err(Socks5Error::AuthFailed)), "{result:?}");
    }

    #[tokio::test]
    async fn refuses_methods_that_were_not_offered() {
        // The proxy wants a password but none is configured.
        let result = scripted(
            Target::Addr(addr("192.0.2.7:1")),
            None,
            vec![(vec![5, 1, 0], vec![5, 2])],
        )
        .await;
        assert!(
            matches!(result, Err(Socks5Error::Protocol(_))),
            "{result:?}"
        );
        let result = scripted(
            Target::Addr(addr("192.0.2.7:1")),
            None,
            vec![(vec![5, 1, 0], vec![5, 0xFF])],
        )
        .await;
        assert!(
            matches!(result, Err(Socks5Error::NoAcceptableMethod)),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn rejects_malformed_replies() {
        let request = vec![5, 1, 0, 1, 192, 0, 2, 7, 0, 1];
        for (reply, what) in [
            (vec![4, 0, 0, 1, 0, 0, 0, 0, 0, 0], "wrong version"),
            (vec![5, 0, 1, 1, 0, 0, 0, 0, 0, 0], "reserved byte"),
            (vec![5, 0, 0, 9, 0, 0, 0, 0, 0, 0], "unknown address type"),
            (vec![5, 0, 0, 3, 0, 0, 0], "empty domain"),
            (vec![5, 0, 0, 1, 0, 0], "truncated"),
        ] {
            let result = scripted(
                Target::Addr(addr("192.0.2.7:1")),
                None,
                vec![(vec![5, 1, 0], vec![5, 0]), (request.clone(), reply)],
            )
            .await;
            assert!(
                matches!(result, Err(Socks5Error::Protocol(_) | Socks5Error::Io(_))),
                "{what}: {result:?}"
            );
        }
        let result = scripted(
            Target::Addr(addr("192.0.2.7:1")),
            None,
            vec![
                (vec![5, 1, 0], vec![0x05, 0x00]),
                (request, vec![5, 5, 0, 1, 0, 0, 0, 0, 0, 0]),
            ],
        )
        .await;
        match result {
            Err(Socks5Error::Refused(reply)) => {
                assert_eq!(reply, Reply::ConnectionRefused);
                assert!(reply.blames_target());
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_unsendable_targets_and_credentials() {
        let long = "a".repeat(256);
        let (mut a, _b) = duplex(64);
        let r = connect(&mut a, &Target::Domain(long.clone(), 1), None).await;
        assert!(matches!(r, Err(Socks5Error::InvalidTarget)));
        let r = connect(&mut a, &Target::Domain(String::new(), 1), None).await;
        assert!(matches!(r, Err(Socks5Error::InvalidTarget)));

        let script = vec![(vec![5, 2, 0, 2], vec![5, 2])];
        let r = scripted(
            Target::Addr(addr("192.0.2.7:1")),
            Some((&long, "p")),
            script,
        )
        .await;
        assert!(matches!(r, Err(Socks5Error::InvalidCredentials)), "{r:?}");
    }

    #[tokio::test]
    async fn server_and_client_agree() {
        let (mut client, mut server) = duplex(1024);
        let server_task = tokio::spawn(async move {
            let auth = ServerAuth::Password {
                username: "lumen",
                password: "token",
            };
            let target = accept(&mut server, auth).await.unwrap();
            send_reply(&mut server, Reply::Succeeded).await.unwrap();
            server.write_all(b"payload").await.unwrap();
            target
        });
        connect(
            &mut client,
            &Target::Domain("example.org".into(), 443),
            Some(("lumen", "token")),
        )
        .await
        .unwrap();
        let mut payload = [0u8; 7];
        client.read_exact(&mut payload).await.unwrap();
        assert_eq!(&payload, b"payload", "nothing past the reply was consumed");
        assert_eq!(
            server_task.await.unwrap(),
            Target::Domain("example.org".into(), 443)
        );
    }

    #[tokio::test]
    async fn server_reports_which_user_authenticated() {
        for (user, expected) in [
            ("lumen", Some(0)),
            ("lumen-public", Some(1)),
            ("other", None),
        ] {
            let (mut client, mut server) = duplex(1024);
            let server_task = tokio::spawn(async move {
                let auth = ServerAuth::AnyUser {
                    usernames: &["lumen", "lumen-public"],
                    password: "token",
                };
                let accepted = accept_as(&mut server, auth).await;
                if accepted.is_ok() {
                    send_reply(&mut server, Reply::Succeeded).await.unwrap();
                }
                accepted.map(|(_, i)| i)
            });
            let r = connect(
                &mut client,
                &Target::Domain("example.org".into(), 443),
                Some((user, "token")),
            )
            .await;
            assert_eq!(r.is_ok(), expected.is_some(), "{user}");
            assert_eq!(server_task.await.unwrap().ok(), expected, "{user}");
        }
    }

    #[tokio::test]
    async fn server_rejects_bad_tokens_and_other_commands() {
        let (mut client, mut server) = duplex(1024);
        let server_task = tokio::spawn(async move {
            let auth = ServerAuth::Password {
                username: "lumen",
                password: "token",
            };
            accept(&mut server, auth).await
        });
        let r = connect(
            &mut client,
            &Target::Addr(addr("192.0.2.1:1")),
            Some(("lumen", "nope")),
        )
        .await;
        assert!(matches!(r, Err(Socks5Error::AuthFailed)), "{r:?}");
        assert!(matches!(
            server_task.await.unwrap(),
            Err(Socks5Error::AuthFailed)
        ));

        // UDP ASSOCIATE (3) is answered with "command not supported".
        let (mut client, mut server) = duplex(1024);
        let server_task = tokio::spawn(async move { accept(&mut server, ServerAuth::None).await });
        client.write_all(&[5, 1, 0]).await.unwrap();
        let mut choice = [0u8; 2];
        client.read_exact(&mut choice).await.unwrap();
        assert_eq!(choice, [5, 0]);
        client
            .write_all(&[5, 3, 0, 1, 0, 0, 0, 0, 0, 0])
            .await
            .unwrap();
        let mut reply = [0u8; 10];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[1], 7);
        assert!(matches!(
            server_task.await.unwrap(),
            Err(Socks5Error::UnsupportedCommand)
        ));
    }
}
