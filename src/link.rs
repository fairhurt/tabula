//! The live link between a running Tabula window and `tabula mcp` processes.
//!
//! The app listens on a local socket (a named pipe on Windows, an abstract
//! Unix socket on Linux). Each `tabula mcp` started by an agent connects when
//! it needs the app: to log what the agent did, to ask you to approve a
//! write, or to open a query in a tab. One JSON object per line each way.

use std::io::{BufRead, BufReader, Write};
use std::time::Duration;

use interprocess::local_socket::{
    GenericFilePath, GenericNamespaced, ListenerOptions, Name, Stream, prelude::*,
};
use serde::{Deserialize, Serialize};

use crate::profiles;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LinkRequest {
    Ping,
    /// An agent used a tool. Shown in the AI activity log.
    Activity {
        agent: String,
        connection: String,
        tool: String,
        detail: String,
        ok: bool,
    },
    /// An agent wants to run a statement that writes. The reply says whether
    /// you allowed it.
    Approve {
        agent: String,
        connection: String,
        sql: String,
    },
    /// Open `sql` in a new query tab and run it.
    Open {
        connection: String,
        sql: String,
        title: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LinkReply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl LinkReply {
    pub fn yes() -> Self {
        LinkReply { ok: true, message: None }
    }
    pub fn no(message: impl Into<String>) -> Self {
        LinkReply {
            ok: false,
            message: Some(message.into()),
        }
    }
}

fn socket_id() -> String {
    // FNV-1a of the config folder, so a test profile gets its own socket.
    let dir = profiles::config_dir();
    let mut h: u64 = 0xcbf29ce484222325;
    for b in dir.to_string_lossy().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("tabula-{:012x}.sock", h & 0xffff_ffff_ffff)
}

fn socket_name() -> std::io::Result<Name<'static>> {
    let id = socket_id();
    if GenericNamespaced::is_supported() {
        id.to_ns_name::<GenericNamespaced>()
    } else {
        std::env::temp_dir()
            .join(id)
            .into_os_string()
            .to_fs_name::<GenericFilePath>()
    }
}

/// Send one request to the running app. `Err` if the app isn't open.
pub fn request(req: &LinkRequest, timeout: Duration) -> anyhow::Result<LinkReply> {
    let conn = Stream::connect(socket_name()?)
        .map_err(|_| anyhow::anyhow!("Tabula isn't open"))?;
    let _ = conn.set_recv_timeout(Some(timeout));
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    (&conn).write_all(line.as_bytes())?;
    let mut reply = String::new();
    BufReader::new(&conn).read_line(&mut reply)?;
    Ok(serde_json::from_str(&reply)?)
}


pub type Incoming = (LinkRequest, futures::channel::oneshot::Sender<LinkReply>);

/// Listen for agents on a background thread. Each request arrives on the
/// returned channel with a sender for its reply.
pub fn listen() -> anyhow::Result<futures::channel::mpsc::UnboundedReceiver<Incoming>> {
    let listener = ListenerOptions::new()
        .name(socket_name()?)
        .try_overwrite(true)
        .create_sync()?;
    let (tx, rx) = futures::channel::mpsc::unbounded::<Incoming>();
    std::thread::Builder::new()
        .name("tabula-link".into())
        .spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { continue };
                let tx = tx.clone();
                std::thread::spawn(move || serve(conn, tx));
            }
        })?;
    Ok(rx)
}

fn serve(conn: Stream, tx: futures::channel::mpsc::UnboundedSender<Incoming>) {
    let mut line = String::new();
    if BufReader::new(&conn).read_line(&mut line).is_err() {
        return;
    }
    let reply = match serde_json::from_str::<LinkRequest>(&line) {
        Err(e) => LinkReply::no(format!("bad request: {e}")),
        Ok(LinkRequest::Ping) => LinkReply::yes(),
        Ok(req) => {
            let (rtx, rrx) = futures::channel::oneshot::channel();
            if tx.unbounded_send((req, rtx)).is_err() {
                LinkReply::no("Tabula is closing")
            } else {
                futures::executor::block_on(rrx).unwrap_or_else(|_| LinkReply::no("no answer"))
            }
        }
    };
    let mut out = serde_json::to_string(&reply).unwrap_or_default();
    out.push('\n');
    let _ = (&conn).write_all(out.as_bytes());
}
