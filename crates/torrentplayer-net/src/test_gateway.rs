// SPDX-License-Identifier: MIT OR Apache-2.0

//! A fake UPnP router on loopback, for tests of port mapping.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Default)]
struct Shared {
    /// The public address reported; `None` refuses every request.
    external: Mutex<Option<IpAddr>>,
    /// The external port of every `AddPortMapping` request, in order.
    mapped: Mutex<Vec<u16>>,
    /// The external port of every `DeletePortMapping` request, in order.
    unmapped: Mutex<Vec<u16>>,
}

/// A router's UPnP `WANIPConnection` service on `127.0.0.1`: accepts every `AddPortMapping` and
/// `DeletePortMapping` and reports a fixed public address, until told to refuse. Stops when
/// dropped.
#[derive(Debug)]
pub struct TestGateway {
    addr: SocketAddr,
    shared: Arc<Shared>,
    cancel: CancellationToken,
}

impl TestGateway {
    /// Starts a router on a free port that reports `external` as its public address.
    ///
    /// # Errors
    ///
    /// Returns an error if no port can be bound.
    pub async fn start(external: IpAddr) -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let gateway = Self {
            addr: listener.local_addr()?,
            shared: Arc::new(Shared {
                external: Mutex::new(Some(external)),
                mapped: Mutex::default(),
                unmapped: Mutex::default(),
            }),
            cancel: CancellationToken::new(),
        };
        let (shared, cancel) = (Arc::clone(&gateway.shared), gateway.cancel.clone());
        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    () = cancel.cancelled() => return,
                    accepted = listener.accept() => accepted,
                };
                if let Ok((stream, _)) = accepted {
                    tokio::spawn(answer(stream, Arc::clone(&shared)));
                }
            }
        });
        Ok(gateway)
    }

    /// The control URL, for [`NetworkOptions::upnp_control_url`](crate::NetworkOptions).
    #[must_use]
    pub fn control_url(&self) -> String {
        format!("http://{}/control", self.addr)
    }

    /// Reports `external` as the public address from now on; `None` refuses every request
    /// (as a router whose UPnP was turned off).
    pub fn set_external(&self, external: Option<IpAddr>) {
        *self
            .shared
            .external
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = external;
    }

    /// The external port of every `AddPortMapping` request so far, refused ones included.
    #[must_use]
    pub fn mapped_ports(&self) -> Vec<u16> {
        self.shared
            .mapped
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The external port of every `DeletePortMapping` request so far, refused ones included.
    #[must_use]
    pub fn unmapped_ports(&self) -> Vec<u16> {
        self.shared
            .unmapped
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Drop for TestGateway {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Answers one SOAP request, then closes the connection.
async fn answer(mut stream: TcpStream, shared: Arc<Shared>) {
    let mut request = Vec::new();
    let mut buf = [0; 4096];
    let body_start = loop {
        let Ok(n) = stream.read(&mut buf).await else {
            return;
        };
        if n == 0 {
            return;
        }
        request.extend_from_slice(&buf[..n]);
        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let head = String::from_utf8_lossy(&request[..body_start]).to_ascii_lowercase();
    let length = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while request.len() < body_start + length {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => request.extend_from_slice(&buf[..n]),
        }
    }
    let body = String::from_utf8_lossy(&request[body_start..]);
    let external = *shared
        .external
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let reply = if head.contains("#addportmapping") {
        if let Some(port) = value(&body, "NewExternalPort").and_then(|p| p.parse().ok()) {
            shared
                .mapped
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(port);
        }
        external.map(|_| String::new())
    } else if head.contains("#deleteportmapping") {
        if let Some(port) = value(&body, "NewExternalPort").and_then(|p| p.parse().ok()) {
            shared
                .unmapped
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(port);
        }
        external.map(|_| String::new())
    } else if head.contains("#getexternalipaddress") {
        external.map(|ip| format!("<NewExternalIPAddress>{ip}</NewExternalIPAddress>"))
    } else {
        None
    };
    let (status, body) = match reply {
        Some(args) => (
            "200 OK",
            format!("<s:Envelope><s:Body>{args}</s:Body></s:Envelope>"),
        ),
        None => (
            "500 Internal Server Error",
            "<s:Envelope><s:Body><errorDescription>Action Failed</errorDescription>\
             </s:Body></s:Envelope>"
                .to_owned(),
        ),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/xml\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn value<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let start = xml.find(&format!("<{tag}>"))? + tag.len() + 2;
    let end = start + xml[start..].find(&format!("</{tag}>"))?;
    Some(&xml[start..end])
}
