//! Shared bits of qrtx: the ticket that goes into the QR code, the pairing
//! protocol spoken between the phone and each device, and the tunnel ALPN.
//!
//! Flow:
//! 1. Each device binds an iroh endpoint and shows a QR code with a [`Ticket`]:
//!    its endpoint id, its home relay, its [`Role`] and a random one-time secret.
//! 2. The phone scans both codes, dials each device on [`ALPN_PAIR`] (which
//!    authenticates the device by its endpoint id) and proves it saw the QR code
//!    by sending the secret ([`PhoneMsg::Auth`]).
//! 3. The phone tells each device about the other ([`PhoneMsg::Pair`]). One of
//!    them dials the other on [`ALPN_PIPE`]; the acceptor only lets the endpoint id
//!    it was told about in. From there on it works like dumbpipe.

use std::{fmt, str::FromStr};

use anyhow::{Context, Result, bail, ensure};
use data_encoding::BASE32_NOPAD;
use iroh::{
    EndpointAddr, EndpointId, RelayUrl,
    endpoint::{RecvStream, SendStream},
};
use serde::{Deserialize, Serialize};

/// ALPN the phone uses to talk to a device.
pub const ALPN_PAIR: &[u8] = b"qrtx/pair/1";
/// ALPN for the device-to-device tunnel.
pub const ALPN_PIPE: &[u8] = b"qrtx/pipe/1";
/// Sent by whoever opens a tunnel stream, so the other side sees the stream
/// even if the opener has nothing to say yet (QUIC streams are lazy).
pub const HANDSHAKE: [u8; 4] = *b"qrtx";
/// Default site that serves the scanner page. Self-hosters can bake in their
/// own with `QRTX_DEFAULT_SITE=https://example.com/ cargo build`.
pub const DEFAULT_SITE: &str = match option_env!("QRTX_DEFAULT_SITE") {
    Some(site) => site,
    None => "https://qrtx.lol/",
};

const TICKET_PREFIX: &str = "Q1";
const SECRET_LEN: usize = 16;
const MAX_FRAME: usize = 64 * 1024;

/// What a device does with the tunnel once it is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    /// Forward stdin/stdout over a single stream.
    Pipe,
    /// Forward every incoming stream to a local TCP service.
    ListenTcp,
    /// Listen on a local TCP port and forward every connection to the peer.
    ConnectTcp,
}

impl Role {
    fn code(self) -> char {
        match self {
            Role::Pipe => 'P',
            Role::ListenTcp => 'L',
            Role::ConnectTcp => 'C',
        }
    }

    fn from_code(c: char) -> Option<Self> {
        match c {
            'P' => Some(Role::Pipe),
            'L' => Some(Role::ListenTcp),
            'C' => Some(Role::ConnectTcp),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Pipe => "pipe",
            Role::ListenTcp => "listen-tcp",
            Role::ConnectTcp => "connect-tcp",
        }
    }

    /// Whether a device with this role opens the tunnel connection to a peer
    /// with role `peer`. `None` means either side may dial (pipe to pipe), and
    /// an error means the two roles can't be paired.
    pub fn dials(self, peer: Role) -> Result<Option<bool>, &'static str> {
        use Role::*;
        match (self, peer) {
            (ListenTcp, ListenTcp) => Err(
                "both devices expose a TCP service (listen-tcp); run `connect-tcp` or `pipe` on one of them",
            ),
            (ConnectTcp, ConnectTcp) => Err(
                "both devices forward a local port (connect-tcp); run `listen-tcp` or `pipe` on one of them",
            ),
            (ConnectTcp, _) | (_, ListenTcp) => Ok(Some(true)),
            (ListenTcp, _) | (_, ConnectTcp) => Ok(Some(false)),
            (Pipe, Pipe) => Ok(None),
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Decide which of two devices dials: returns `true` if `a` dials `b`.
/// For two pipes the second device scanned (`b`) dials.
pub fn a_dials(a: Role, b: Role) -> Result<bool, &'static str> {
    Ok(a.dials(b)?.unwrap_or(false))
}

/// Everything the phone needs to reach and authenticate to a device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ticket {
    pub id: EndpointId,
    pub role: Role,
    pub secret: [u8; SECRET_LEN],
    pub relay: Option<RelayUrl>,
}

impl Ticket {
    pub fn new(
        id: EndpointId,
        role: Role,
        secret: [u8; SECRET_LEN],
        relay: Option<RelayUrl>,
    ) -> Self {
        Self {
            id,
            role,
            secret,
            relay,
        }
    }

    pub fn random_secret() -> [u8; SECRET_LEN] {
        let mut secret = [0u8; SECRET_LEN];
        getrandom_fill(&mut secret);
        secret
    }

    pub fn addr(&self) -> EndpointAddr {
        endpoint_addr(self.id, self.relay.clone())
    }

    /// The URL that goes into the QR code. The ticket lives in the fragment, so
    /// it never reaches the web server.
    ///
    /// The payload sticks to the QR alphanumeric charset (`0-9A-Z.-:` etc.) so the
    /// encoder can pack it densely, which keeps the code small enough for a terminal.
    pub fn to_url(&self, site: &str) -> String {
        let mut url = String::with_capacity(site.len() + 140);
        url.push_str(site);
        if !site.ends_with('/') && !site.ends_with('#') {
            url.push('/');
        }
        if !url.ends_with('#') {
            url.push('#');
        }
        url.push_str(&self.payload());
        url
    }

    pub fn payload(&self) -> String {
        let mut out = format!(
            "{TICKET_PREFIX}{}.{}.{}",
            self.role.code(),
            BASE32_NOPAD.encode(self.id.as_bytes()),
            BASE32_NOPAD.encode(&self.secret)
        );
        if let Some(relay) = &self.relay {
            out.push('.');
            out.push_str(&encode_relay(relay));
        }
        out
    }

    pub fn secret_b32(&self) -> String {
        BASE32_NOPAD.encode(&self.secret)
    }
}

impl FromStr for Ticket {
    type Err = anyhow::Error;

    /// Accepts the full URL, just the fragment, or the bare payload.
    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim();
        let payload = s.rsplit_once('#').map_or(s, |(_, frag)| frag);
        let mut parts = payload.splitn(4, '.');
        let head = parts.next().unwrap_or_default().to_ascii_uppercase();
        let Some(role) = head.strip_prefix(TICKET_PREFIX) else {
            bail!("not a qrtx code");
        };
        let mut role_chars = role.chars();
        let role = match (
            role_chars.next().and_then(Role::from_code),
            role_chars.next(),
        ) {
            (Some(role), None) => role,
            _ => bail!("unknown device role in qrtx code"),
        };
        let id = parts.next().context("qrtx code is missing the device id")?;
        let id = decode_b32(id).context("invalid device id")?;
        let id: [u8; 32] = id.try_into().ok().context("invalid device id length")?;
        let id = EndpointId::from_bytes(&id).context("invalid device id")?;
        let secret = parts.next().context("qrtx code is missing the secret")?;
        let secret = decode_b32(secret).context("invalid secret")?;
        let secret: [u8; SECRET_LEN] = secret.try_into().ok().context("invalid secret length")?;
        let relay = parts
            .next()
            .filter(|r| !r.is_empty())
            .map(decode_relay)
            .transpose()?;
        Ok(Self {
            id,
            role,
            secret,
            relay,
        })
    }
}

impl fmt::Display for Ticket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.payload())
    }
}

pub fn endpoint_addr(id: EndpointId, relay: Option<RelayUrl>) -> EndpointAddr {
    let addr = EndpointAddr::new(id);
    match relay {
        Some(relay) => addr.with_relay_url(relay),
        None => addr,
    }
}

pub fn encode_id(id: &EndpointId) -> String {
    BASE32_NOPAD.encode(id.as_bytes())
}

pub fn decode_id(s: &str) -> Result<EndpointId> {
    let bytes: [u8; 32] = decode_b32(s)?
        .try_into()
        .ok()
        .context("invalid endpoint id length")?;
    Ok(EndpointId::from_bytes(&bytes)?)
}

/// Constant-time comparison of a presented secret against ours.
pub fn secret_matches(ours: &[u8; SECRET_LEN], presented: &str) -> bool {
    let Ok(presented) = decode_b32(presented) else {
        return false;
    };
    if presented.len() != SECRET_LEN {
        return false;
    }
    ours.iter()
        .zip(presented.iter())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

fn decode_b32(s: &str) -> Result<Vec<u8>> {
    Ok(BASE32_NOPAD.decode(s.to_ascii_uppercase().as_bytes())?)
}

/// The common case (`https://host[:port]/`) is encoded as just the uppercased host,
/// which stays in the QR alphanumeric charset. Anything else is kept verbatim.
fn encode_relay(relay: &RelayUrl) -> String {
    let plain = relay.scheme() == "https"
        && relay.path() == "/"
        && relay.query().is_none()
        && relay.username().is_empty()
        && relay.password().is_none();
    match (plain, relay.host_str()) {
        (true, Some(host)) => {
            let mut out = host.to_ascii_uppercase();
            if let Some(port) = relay.port() {
                out.push_str(&format!(":{port}"));
            }
            out
        }
        _ => relay.to_string(),
    }
}

fn decode_relay(s: &str) -> Result<RelayUrl> {
    let url = if s.contains("://") {
        s.to_string()
    } else {
        format!("https://{}/", s.to_ascii_lowercase())
    };
    RelayUrl::from_str(&url).with_context(|| format!("invalid relay url {url:?}"))
}

fn getrandom_fill(buf: &mut [u8]) {
    // A fresh iroh secret key is 32 bytes of OS randomness; borrow some of it
    // instead of pulling in another rng crate.
    let mut filled = 0;
    while filled < buf.len() {
        let key = iroh::SecretKey::generate().to_bytes();
        let n = (buf.len() - filled).min(key.len());
        buf[filled..filled + n].copy_from_slice(&key[..n]);
        filled += n;
    }
}

/// Phone -> device.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "kebab-case")]
pub enum PhoneMsg {
    /// First message: the secret from the QR code.
    Auth { secret: String },
    /// Who to tunnel with, and whether to dial or wait for them.
    Pair {
        id: String,
        relay: Option<String>,
        role: Role,
        name: String,
        dial: bool,
    },
}

/// Device -> phone.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "kebab-case")]
pub enum DeviceMsg {
    /// Reply to a valid [`PhoneMsg::Auth`].
    Welcome {
        role: Role,
        name: String,
        detail: String,
        version: String,
    },
    /// The device accepted the [`PhoneMsg::Pair`] and is dialing/waiting.
    Ready,
    /// The tunnel to the peer is up.
    Connected {
        direct: bool,
    },
    Error {
        message: String,
    },
}

pub async fn write_msg<T: Serialize>(send: &mut SendStream, msg: &T) -> Result<()> {
    let body = serde_json::to_vec(msg)?;
    ensure!(body.len() <= MAX_FRAME, "message too large");
    send.write_all(&(body.len() as u32).to_be_bytes()).await?;
    send.write_all(&body).await?;
    Ok(())
}

/// Reads one message; `Ok(None)` if the stream ended cleanly before it.
pub async fn read_msg<T: for<'de> Deserialize<'de>>(recv: &mut RecvStream) -> Result<Option<T>> {
    let mut len = [0u8; 4];
    match recv.read(&mut len[..1]).await? {
        None => return Ok(None),
        Some(0) => bail!("unexpected empty read"),
        Some(_) => {}
    }
    recv.read_exact(&mut len[1..]).await?;
    let len = u32::from_be_bytes(len) as usize;
    ensure!(len <= MAX_FRAME, "message too large ({len} bytes)");
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body).await?;
    Ok(Some(serde_json::from_slice(&body)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket(role: Role, relay: Option<&str>) -> Ticket {
        Ticket::new(
            iroh::SecretKey::generate().public(),
            role,
            Ticket::random_secret(),
            relay.map(|r| r.parse().unwrap()),
        )
    }

    #[test]
    fn roundtrip_n0_relay() {
        let t = ticket(
            Role::ListenTcp,
            Some("https://euc1-1.relay.n0.iroh-canary.iroh.link./"),
        );
        let url = t.to_url(DEFAULT_SITE);
        assert!(url.starts_with("https://qrtx.lol/#Q1L."));
        let payload = url.split_once('#').unwrap().1;
        // stays within the QR alphanumeric charset
        assert!(
            payload
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || ".-:".contains(c))
        );
        assert_eq!(url.parse::<Ticket>().unwrap(), t);
        assert_eq!(payload.parse::<Ticket>().unwrap(), t);
        assert_eq!(payload.to_ascii_lowercase().parse::<Ticket>().unwrap(), t);
    }

    #[test]
    fn roundtrip_odd_relays() {
        for relay in [
            None,
            Some("https://relay.example.com:8443/"),
            Some("http://localhost:3340/"),
            Some("https://relay.example.com/some/Path"),
        ] {
            let t = ticket(Role::Pipe, relay);
            assert_eq!(
                t.to_url("http://localhost:8000").parse::<Ticket>().unwrap(),
                t,
                "{relay:?}"
            );
        }
    }

    #[test]
    fn rejects_garbage() {
        assert!("https://example.com/".parse::<Ticket>().is_err());
        assert!("Q1X.AAAA.BBBB".parse::<Ticket>().is_err());
        assert!("Q1P.AAAA.BBBB".parse::<Ticket>().is_err());
    }

    #[test]
    fn secret_check() {
        let t = ticket(Role::Pipe, None);
        assert!(secret_matches(&t.secret, &t.secret_b32()));
        assert!(secret_matches(&t.secret, &t.secret_b32().to_lowercase()));
        assert!(!secret_matches(
            &t.secret,
            &Ticket::new(t.id, t.role, [0; 16], None).secret_b32()
        ));
        assert!(!secret_matches(&t.secret, "AAAA"));
    }

    #[test]
    fn dial_plan() {
        use Role::*;
        assert_eq!(a_dials(Pipe, Pipe), Ok(false));
        assert_eq!(a_dials(ConnectTcp, ListenTcp), Ok(true));
        assert_eq!(a_dials(ListenTcp, ConnectTcp), Ok(false));
        assert_eq!(a_dials(Pipe, ListenTcp), Ok(true));
        assert_eq!(a_dials(ListenTcp, Pipe), Ok(false));
        assert_eq!(a_dials(Pipe, ConnectTcp), Ok(false));
        assert!(a_dials(ListenTcp, ListenTcp).is_err());
        assert!(a_dials(ConnectTcp, ConnectTcp).is_err());
    }
}
