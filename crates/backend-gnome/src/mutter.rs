//! Mutter's private remote-desktop D-Bus API, as used by this backend.
//!
//! Interfaces (Mutter 50.5, spike S0): `org.gnome.Mutter.RemoteDesktop` v1,
//! `org.gnome.Mutter.ScreenCast` v4, `org.gnome.Mutter.DisplayConfig`.
//! Mutter documents them as private with no compatibility promise, so
//! everything we rely on is listed here and checked by preflight.
//!
//! Mutter ties a session to the D-Bus connection that created it and stops
//! it when that connection's name vanishes (also on a crash, S1). So one
//! connection owns the whole session, and `ConnectToEIS` must go over that
//! same connection (`Owner::eis_fd`).

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::time::Duration;

use anyhow::{anyhow, Context as _};
use futures_util::StreamExt as _;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream};

use crate::layout::{Logical, LogicalConfig, Monitor, State};

pub const RD: &str = "org.gnome.Mutter.RemoteDesktop";
pub const RD_PATH: &str = "/org/gnome/Mutter/RemoteDesktop";
pub const RD_SESSION: &str = "org.gnome.Mutter.RemoteDesktop.Session";
pub const SC: &str = "org.gnome.Mutter.ScreenCast";
pub const SC_PATH: &str = "/org/gnome/Mutter/ScreenCast";
pub const SC_SESSION: &str = "org.gnome.Mutter.ScreenCast.Session";
pub const SC_STREAM: &str = "org.gnome.Mutter.ScreenCast.Stream";
pub const DC: &str = "org.gnome.Mutter.DisplayConfig";
pub const DC_PATH: &str = "/org/gnome/Mutter/DisplayConfig";

/// `cursor-mode` for RecordVirtual: 0 hidden, 1 embedded, 2 metadata.
const CURSOR_EMBEDDED: u32 = 1;
/// `ApplyMonitorsConfig` method: 0 verify, 1 temporary, 2 persistent.
const METHOD_TEMPORARY: u32 = 1;
/// How long Mutter may take to publish the PipeWire node after Start.
const NODE_TIMEOUT: Duration = Duration::from_secs(5);

/// A live RemoteDesktop + ScreenCast session with one virtual monitor.
pub struct Owner {
    pub conn: Connection,
    pub rd: OwnedObjectPath,
    pub stream: OwnedObjectPath,
    pub node: u32,
    /// Connectors that existed before RecordVirtual (to spot ours).
    pub before: Vec<String>,
    /// Mutter's `Closed` signal for this session.
    pub closed: MessageStream,
}

async fn call<B, R>(
    conn: &Connection,
    dest: &str,
    path: &str,
    iface: &str,
    method: &str,
    body: &B,
) -> anyhow::Result<R>
where
    B: serde::ser::Serialize + zbus::zvariant::DynamicType,
    R: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
{
    let reply = conn
        .call_method(Some(dest), path, Some(iface), method, body)
        .await
        .with_context(|| format!("{iface}.{method}"))?;
    reply
        .body()
        .deserialize::<R>()
        .with_context(|| format!("{iface}.{method} reply"))
}

async fn signal_stream(
    conn: &Connection,
    iface: &str,
    member: &str,
    path: Option<&str>,
) -> anyhow::Result<MessageStream> {
    let mut b = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(iface.to_string())?
        .member(member.to_string())?;
    if let Some(p) = path {
        b = b.path(p.to_string())?;
    }
    Ok(MessageStream::for_match_rule(b.build(), conn, None).await?)
}

/// Create the session pair, the virtual monitor, start it and wait for the
/// PipeWire node. On error everything created so far is stopped.
pub async fn open() -> anyhow::Result<Owner> {
    let conn = Connection::session().await.context("session bus")?;
    let (rd,): (OwnedObjectPath,) = call(&conn, RD, RD_PATH, RD, "CreateSession", &()).await?;
    let result = open_on(&conn, &rd).await;
    if result.is_err() {
        let _ = call::<_, ()>(&conn, RD, rd.as_str(), RD_SESSION, "Stop", &()).await;
    }
    let (stream, node, before, closed) = result?;
    Ok(Owner {
        conn,
        rd,
        stream,
        node,
        before,
        closed,
    })
}

async fn open_on(
    conn: &Connection,
    rd: &OwnedObjectPath,
) -> anyhow::Result<(OwnedObjectPath, u32, Vec<String>, MessageStream)> {
    let id: String = property(conn, RD, rd.as_str(), RD_SESSION, "SessionId").await?;
    let opts: HashMap<&str, Value> =
        HashMap::from([("remote-desktop-session-id", Value::from(id))]);
    let (sc,): (OwnedObjectPath,) = call(conn, SC, SC_PATH, SC, "CreateSession", &(opts,)).await?;
    let before = state(conn).await?.connectors();
    let rec: HashMap<&str, Value> = HashMap::from([
        ("cursor-mode", Value::from(CURSOR_EMBEDDED)),
        ("is-platform", Value::from(true)),
    ]);
    let (stream,): (OwnedObjectPath,) =
        call(conn, SC, sc.as_str(), SC_SESSION, "RecordVirtual", &(rec,)).await?;
    // Subscribe before Start: the node can be announced right away.
    let mut added = signal_stream(
        conn,
        SC_STREAM,
        "PipeWireStreamAdded",
        Some(stream.as_str()),
    )
    .await?;
    let closed = signal_stream(conn, RD_SESSION, "Closed", Some(rd.as_str())).await?;
    call::<_, ()>(conn, RD, rd.as_str(), RD_SESSION, "Start", &()).await?;
    let msg = tokio::time::timeout(NODE_TIMEOUT, added.next())
        .await
        .map_err(|_| anyhow!("no PipeWireStreamAdded within {NODE_TIMEOUT:?}"))?
        .ok_or_else(|| anyhow!("signal stream ended"))??;
    let (node,): (u32,) = msg.body().deserialize()?;
    Ok((stream, node, before, closed))
}

impl Owner {
    /// Stop the session; Mutter removes the virtual monitor.
    pub async fn stop(&self) {
        if let Err(e) =
            call::<_, ()>(&self.conn, RD, self.rd.as_str(), RD_SESSION, "Stop", &()).await
        {
            eprintln!("[backend-gnome] session: RemoteDesktop.Stop: {e:#}");
        }
    }

    /// The stream's `mapping-id` (EIS regions carry it), if Mutter sets one.
    pub async fn mapping_id(&self) -> Option<String> {
        let params: HashMap<String, OwnedValue> = property(
            &self.conn,
            SC,
            self.stream.as_str(),
            SC_STREAM,
            "Parameters",
        )
        .await
        .ok()?;
        params
            .get("mapping-id")
            .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
    }

    /// A blocking handle on the owning connection plus the session path, for
    /// the input thread's `ConnectToEIS`.
    pub fn eis_target(&self) -> EisTarget {
        EisTarget {
            conn: zbus::blocking::Connection::from(self.conn.clone()),
            rd: self.rd.clone(),
        }
    }
}

/// What the input thread needs to ask Mutter for an EIS socket.
#[derive(Clone)]
pub struct EisTarget {
    conn: zbus::blocking::Connection,
    rd: OwnedObjectPath,
}

impl EisTarget {
    pub fn eis_fd(&self) -> anyhow::Result<OwnedFd> {
        let opts: HashMap<&str, Value> = HashMap::new();
        let reply = self
            .conn
            .call_method(
                Some(RD),
                self.rd.as_str(),
                Some(RD_SESSION),
                "ConnectToEIS",
                &(opts,),
            )
            .context("RemoteDesktop.Session.ConnectToEIS")?;
        let (fd,): (zbus::zvariant::OwnedFd,) = reply.body().deserialize()?;
        Ok(fd.into())
    }

    pub fn label(&self) -> String {
        self.rd
            .as_str()
            .rsplit('/')
            .next()
            .unwrap_or("?")
            .to_string()
    }
}

async fn property<T>(
    conn: &Connection,
    dest: &str,
    path: &str,
    iface: &str,
    name: &str,
) -> anyhow::Result<T>
where
    T: TryFrom<OwnedValue>,
    <T as TryFrom<OwnedValue>>::Error: Into<zbus::zvariant::Error>,
{
    let (v,): (OwnedValue,) = call(
        conn,
        dest,
        path,
        "org.freedesktop.DBus.Properties",
        "Get",
        &(iface, name),
    )
    .await?;
    T::try_from(v).map_err(|e| anyhow!("{iface}.{name}: {}", e.into()))
}

type Mode = (
    String,
    i32,
    i32,
    f64,
    f64,
    Vec<f64>,
    HashMap<String, OwnedValue>,
);
type MonitorRaw = (
    (String, String, String, String),
    Vec<Mode>,
    HashMap<String, OwnedValue>,
);
type LogicalRaw = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<(String, String, String, String)>,
    HashMap<String, OwnedValue>,
);

fn flag(props: &HashMap<String, OwnedValue>, key: &str) -> bool {
    props
        .get(key)
        .and_then(|v| bool::try_from(&**v).ok())
        .unwrap_or(false)
}

/// `DisplayConfig.GetCurrentState`, reduced to what the guard needs.
pub async fn state(conn: &Connection) -> anyhow::Result<State> {
    let (serial, monitors, logical, _props): (
        u32,
        Vec<MonitorRaw>,
        Vec<LogicalRaw>,
        HashMap<String, OwnedValue>,
    ) = call(conn, DC, DC_PATH, DC, "GetCurrentState", &()).await?;
    let monitors = monitors
        .into_iter()
        .map(|((connector, ..), modes, _)| {
            let current = modes.iter().find(|m| flag(&m.6, "is-current"));
            let preferred = modes.iter().find(|m| flag(&m.6, "is-preferred"));
            Monitor {
                connector,
                current_mode: current.map(|m| m.0.clone()),
                preferred_mode: preferred.map(|m| m.0.clone()),
                current_size: current.map(|m| (m.1.max(0) as u32, m.2.max(0) as u32)),
            }
        })
        .collect();
    let logical = logical
        .into_iter()
        .map(|(x, y, scale, _t, _p, mons, _)| Logical {
            x,
            y,
            scale,
            connectors: mons.into_iter().map(|m| m.0).collect(),
        })
        .collect();
    Ok(State {
        serial,
        monitors,
        logical,
    })
}

/// `DisplayConfig.ApplyMonitorsConfig` with the temporary method.
pub async fn apply(conn: &Connection, serial: u32, config: &[LogicalConfig]) -> anyhow::Result<()> {
    type MonCfg<'a> = (&'a str, &'a str, HashMap<&'a str, Value<'a>>);
    let logical: Vec<(i32, i32, f64, u32, bool, Vec<MonCfg<'_>>)> = config
        .iter()
        .map(|l| {
            (
                l.x,
                l.y,
                l.scale,
                0u32,
                l.primary,
                l.monitors
                    .iter()
                    .map(|(c, m)| (c.as_str(), m.as_str(), HashMap::new()))
                    .collect(),
            )
        })
        .collect();
    let props: HashMap<&str, Value> = HashMap::new();
    call::<_, ()>(
        conn,
        DC,
        DC_PATH,
        DC,
        "ApplyMonitorsConfig",
        &(serial, METHOD_TEMPORARY, logical, props),
    )
    .await
}

/// `DisplayConfig.MonitorsChanged`, for re-checking the guard.
pub async fn monitors_changed(conn: &Connection) -> anyhow::Result<MessageStream> {
    signal_stream(conn, DC, "MonitorsChanged", Some(DC_PATH)).await
}
