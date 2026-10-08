//! The phone side of qrtx, compiled to wasm.
//!
//! The browser can only reach iroh endpoints through relays, which is plenty
//! for the few small messages needed to introduce the two devices. The devices
//! then connect to each other directly.
use std::{cell::RefCell, rc::Rc, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use iroh::{
    Endpoint, RelayMode, RelayUrl,
    endpoint::{Connection, RecvStream, SendStream, presets},
};
use n0_future::time::{Instant, timeout};
use qrtx_proto::{
    ALPN_PAIR, DeviceMsg, PhoneMsg, Role, Ticket, encode_id, endpoint_addr, read_msg, write_msg,
};
use tracing::{info, warn};
use tracing_subscriber::{Layer, filter::Targets, fmt::MakeWriter, layer::SubscriberExt, util::SubscriberInitExt};
use serde::Serialize;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::future_to_promise;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(25);
const REPLY_TIMEOUT: Duration = Duration::from_secs(20);
const TUNNEL_TIMEOUT: Duration = Duration::from_secs(120);

#[wasm_bindgen(start)]
fn start() {
    console_error_panic_hook::set_once();
}

thread_local! {
    static LOG_SINK: RefCell<Option<js_sys::Function>> = const { RefCell::new(None) };
}

/// One formatted log event, handed to JS when dropped.
struct JsLine(Vec<u8>);

impl std::io::Write for JsLine {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for JsLine {
    fn drop(&mut self) {
        let line = String::from_utf8_lossy(&self.0);
        let line = line.trim_end();
        if line.is_empty() {
            return;
        }
        LOG_SINK.with(|sink| {
            if let Some(f) = &*sink.borrow() {
                f.call1(&JsValue::NULL, &JsValue::from_str(line)).ok();
            }
        });
    }
}

struct JsMakeWriter;

impl<'a> MakeWriter<'a> for JsMakeWriter {
    type Writer = JsLine;

    fn make_writer(&'a self) -> JsLine {
        JsLine(Vec::new())
    }
}

/// Forward Rust logs (iroh's included) to `sink(line)`. `filter` is a
/// tracing `Targets` string like `warn,iroh=debug`.
#[wasm_bindgen(js_name = setLogger)]
pub fn set_logger(sink: js_sys::Function, filter: &str) -> Result<(), JsValue> {
    LOG_SINK.with(|s| *s.borrow_mut() = Some(sink));
    let targets: Targets = filter.parse().map_err(|e| js_err(anyhow!("{e}")))?;
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(JsMakeWriter)
        .without_time()
        .with_ansi(false)
        .with_filter(targets);
    tracing_subscriber::registry().with(layer).try_init().ok();
    Ok(())
}

/// iroh's relay hostnames are fully qualified (`relay.example.`). Native code
/// doesn't care, but browsers (WebKit in particular) can trip over the
/// trailing dot in TLS/WebSocket URLs, so we drop it in the browser.
fn browser_relay(url: RelayUrl, keep_dots: bool) -> RelayUrl {
    if keep_dots {
        return url;
    }
    let mut u: url::Url = url.into();
    if let Some(host) = u.host_str().and_then(|h| h.strip_suffix('.')).map(str::to_owned) {
        u.set_host(Some(&host)).ok();
    }
    u.into()
}

fn js_err(err: anyhow::Error) -> JsValue {
    JsError::new(&format!("{err:#}")).into()
}

fn to_js<T: Serialize>(value: &T) -> JsValue {
    value
        .serialize(&serde_wasm_bindgen::Serializer::json_compatible())
        .unwrap_or(JsValue::NULL)
}

#[derive(Serialize)]
struct TicketInfo {
    id: String,
    short: String,
    role: Role,
    relay: Option<String>,
}

/// Parse a scanned code (the full URL or just the fragment).
#[wasm_bindgen(js_name = parseTicket)]
pub fn parse_ticket(s: &str) -> Result<JsValue, JsValue> {
    let t: Ticket = s.parse().map_err(js_err)?;
    Ok(to_js(&TicketInfo {
        id: encode_id(&t.id),
        short: t.id.fmt_short().to_string(),
        role: t.role,
        relay: t.relay.map(|r| r.to_string()),
    }))
}

/// Whether device `a` should dial device `b`; throws if the roles don't fit.
#[wasm_bindgen(js_name = aDials)]
pub fn a_dials(role_a: JsValue, role_b: JsValue) -> Result<bool, JsValue> {
    let role =
        |v: JsValue| serde_wasm_bindgen::from_value::<Role>(v).map_err(|e| js_err(anyhow!("{e}")));
    qrtx_proto::a_dials(role(role_a)?, role(role_b)?).map_err(|e| js_err(anyhow!(e)))
}

/// Find a QR code in a greyscale frame, for browsers without `BarcodeDetector`.
#[wasm_bindgen(js_name = decodeQr)]
pub fn decode_qr(luma: &[u8], width: usize, height: usize) -> Option<String> {
    if luma.len() < width * height {
        return None;
    }
    let mut img =
        rqrr::PreparedImage::prepare_from_greyscale(width, height, |x, y| luma[y * width + x]);
    img.detect_grids()
        .into_iter()
        .find_map(|grid| grid.decode().ok().map(|(_, content)| content))
}

/// Our own (throwaway) iroh endpoint.
#[wasm_bindgen]
pub struct Node {
    endpoint: Endpoint,
    keep_dots: bool,
}

#[wasm_bindgen]
impl Node {
    /// `keep_dots` keeps relay hostnames as iroh has them (for diagnosis).
    pub async fn create(keep_dots: bool) -> Result<Node, JsValue> {
        let relays: Vec<RelayUrl> = iroh::defaults::prod::default_relay_map()
            .urls::<Vec<_>>()
            .into_iter()
            .map(|u| browser_relay(u, keep_dots))
            .collect();
        info!("binding endpoint, relays: {}", relays.iter().map(|r| r.to_string()).collect::<Vec<_>>().join(" "));
        let started = Instant::now();
        let endpoint = Endpoint::builder(presets::N0)
            .relay_mode(RelayMode::custom(relays))
            .bind()
            .await
            .map_err(|e| {
                warn!("binding the endpoint failed: {e:#}");
                js_err(e.into())
            })?;
        info!("endpoint {} bound in {:?}", endpoint.id().fmt_short(), started.elapsed());
        // report whether we can reach any relay at all; without one nothing works
        let ep = endpoint.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let started = Instant::now();
            match timeout(Duration::from_secs(15), ep.online()).await {
                Ok(()) => info!(
                    "home relay {} connected after {:?}",
                    ep.addr().relay_urls().next().map(|r| r.to_string()).unwrap_or_default(),
                    started.elapsed()
                ),
                Err(_) => warn!(
                    "no relay connection after 15s: this browser can't open a WebSocket to any iroh relay, so it can't reach your computers"
                ),
            }
        });
        Ok(Node { endpoint, keep_dots })
    }

    /// The relay this browser is connected through, if any.
    #[wasm_bindgen(js_name = homeRelay)]
    pub fn home_relay(&self) -> Option<String> {
        self.endpoint.addr().relay_urls().next().map(|r| r.to_string())
    }

    pub fn id(&self) -> String {
        self.endpoint.id().fmt_short().to_string()
    }

    /// Resolves once we have a home relay (not required for dialing, but a
    /// good sign that networking works).
    pub fn online(&self) -> js_sys::Promise {
        let endpoint = self.endpoint.clone();
        future_to_promise(async move {
            endpoint.online().await;
            Ok(JsValue::TRUE)
        })
    }

    /// Dial the device from a scanned code and authenticate with its secret.
    /// Resolves to a [`Device`].
    pub fn connect(&self, ticket: String) -> js_sys::Promise {
        let endpoint = self.endpoint.clone();
        let keep_dots = self.keep_dots;
        future_to_promise(async move {
            let ticket: Ticket = ticket.parse().map_err(js_err)?;
            let short = ticket.id.fmt_short();
            match Device::connect(endpoint, ticket, keep_dots).await {
                Ok(device) => Ok(device.into()),
                Err(err) => {
                    warn!("device {short}: {err:#}");
                    Err(js_err(err))
                }
            }
        })
    }
}

struct Inner {
    ticket: Ticket,
    conn: Connection,
    send: RefCell<Option<SendStream>>,
    recv: RefCell<Option<RecvStream>>,
    role: Role,
    name: String,
    detail: String,
    version: String,
}

/// An authenticated pairing session with one device.
#[wasm_bindgen]
#[derive(Clone)]
pub struct Device {
    inner: Rc<Inner>,
}

#[derive(Serialize)]
struct DeviceInfo<'a> {
    id: String,
    short: String,
    role: Role,
    name: &'a str,
    detail: &'a str,
    version: &'a str,
}

impl Device {
    async fn connect(endpoint: Endpoint, ticket: Ticket, keep_dots: bool) -> Result<Device> {
        let short = ticket.id.fmt_short();
        let relay = ticket.relay.clone().map(|r| browser_relay(r, keep_dots));
        info!(
            "device {short}: dialing via relay {}",
            relay.as_ref().map(|r| r.to_string()).unwrap_or_else(|| "(none, using discovery)".into())
        );
        let started = Instant::now();
        let addr = endpoint_addr(ticket.id, relay);
        let conn = timeout(CONNECT_TIMEOUT, endpoint.connect(addr, ALPN_PAIR))
            .await
            .context("timed out reaching the device; is qrtx still running there?")?
            .context("could not reach the device")?;
        info!("device {short}: connected after {:?}", started.elapsed());
        let (mut send, mut recv) = conn.open_bi().await?;
        write_msg(
            &mut send,
            &PhoneMsg::Auth {
                secret: ticket.secret_b32(),
            },
        )
        .await?;
        let reply = timeout(REPLY_TIMEOUT, read_msg::<DeviceMsg>(&mut recv))
            .await
            .context("the device did not answer")??;
        let (role, name, detail, version) = match reply {
            Some(DeviceMsg::Welcome {
                role,
                name,
                detail,
                version,
            }) => (role, name, detail, version),
            Some(DeviceMsg::Error { message }) => bail!(message),
            Some(other) => bail!("unexpected reply {other:?}"),
            None => bail!("the device hung up"),
        };
        info!("device {short}: authenticated as {name:?} ({role}) after {:?}", started.elapsed());
        if role != ticket.role {
            bail!(
                "device reports role {role} but its code says {}",
                ticket.role
            );
        }
        Ok(Device {
            inner: Rc::new(Inner {
                ticket,
                conn,
                send: RefCell::new(Some(send)),
                recv: RefCell::new(Some(recv)),
                role,
                name,
                detail,
                version,
            }),
        })
    }

    /// Run `f` with the session streams taken out, then put them back.
    async fn with_streams<T>(
        &self,
        f: impl AsyncFnOnce(&mut SendStream, &mut RecvStream) -> Result<T>,
    ) -> Result<T> {
        let send = self.inner.send.borrow_mut().take();
        let recv = self.inner.recv.borrow_mut().take();
        let (Some(mut send), Some(mut recv)) = (send, recv) else {
            bail!("device session is busy or closed");
        };
        let res = f(&mut send, &mut recv).await;
        *self.inner.send.borrow_mut() = Some(send);
        *self.inner.recv.borrow_mut() = Some(recv);
        res
    }

    async fn reply(recv: &mut RecvStream, wait: Duration) -> Result<DeviceMsg> {
        match timeout(wait, read_msg::<DeviceMsg>(recv))
            .await
            .context("the device did not answer")??
        {
            Some(DeviceMsg::Error { message }) => bail!(message),
            Some(msg) => Ok(msg),
            None => bail!("the device hung up"),
        }
    }
}

#[wasm_bindgen]
impl Device {
    /// `{id, short, role, name, detail, version}` as reported by the device.
    pub fn info(&self) -> JsValue {
        let i = &self.inner;
        to_js(&DeviceInfo {
            id: encode_id(&i.ticket.id),
            short: i.ticket.id.fmt_short().to_string(),
            role: i.role,
            name: &i.name,
            detail: &i.detail,
            version: &i.version,
        })
    }

    /// Tell this device about `peer`. Resolves once the device has accepted.
    pub fn pair(&self, peer: &Device, dial: bool) -> js_sys::Promise {
        let this = self.clone();
        let p = &peer.inner;
        let msg = PhoneMsg::Pair {
            id: encode_id(&p.ticket.id),
            relay: p.ticket.relay.as_ref().map(|r| r.to_string()),
            role: p.role,
            name: p.name.clone(),
            dial,
        };
        future_to_promise(async move {
            this.with_streams(async |send, recv| {
                write_msg(send, &msg).await?;
                match Self::reply(recv, REPLY_TIMEOUT).await? {
                    DeviceMsg::Ready => Ok(()),
                    other => bail!("unexpected reply {other:?}"),
                }
            })
            .await
            .map_err(js_err)?;
            Ok(JsValue::TRUE)
        })
    }

    /// Resolves to `{direct}` once the device reports the tunnel is up.
    #[wasm_bindgen(js_name = waitConnected)]
    pub fn wait_connected(&self) -> js_sys::Promise {
        let this = self.clone();
        future_to_promise(async move {
            let direct = this
                .with_streams(
                    async |_send, recv| match Self::reply(recv, TUNNEL_TIMEOUT).await? {
                        DeviceMsg::Connected { direct } => Ok(direct),
                        other => bail!("unexpected reply {other:?}"),
                    },
                )
                .await
                .map_err(js_err)?;
            #[derive(Serialize)]
            struct Connected {
                direct: bool,
            }
            Ok(to_js(&Connected { direct }))
        })
    }

    pub fn close(&self) {
        if let Some(mut send) = self.inner.send.borrow_mut().take() {
            send.finish().ok();
        }
        self.inner.conn.close(0u32.into(), b"bye");
    }
}
