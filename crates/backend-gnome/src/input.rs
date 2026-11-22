//! GNOME input: Mutter's `RemoteDesktop.Session.ConnectToEIS`, fed to the
//! shared `input-eis` thread.
//!
//! The EIS socket belongs to one remote-desktop session and must be asked
//! for on the connection that owns it, so the connector reads the current
//! target, which the session thread swaps when it recreates the monitor.
//! The old socket then hangs up and the input thread reconnects.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use desk_backend::InputEvent;
use input_eis::{Config, EisLink};

use crate::mutter::EisTarget;

const CONFIG: Config = Config {
    thread: "gnome-input",
    tag: "[backend-gnome]",
};

pub struct Input {
    inner: input_eis::Input,
    target: Arc<Mutex<Option<EisTarget>>>,
    /// Set once the client has injected anything.
    pub input_seen: Arc<AtomicBool>,
}

impl Input {
    pub fn spawn() -> std::io::Result<Self> {
        let target: Arc<Mutex<Option<EisTarget>>> = Arc::default();
        let t = target.clone();
        let connector = move || -> anyhow::Result<EisLink> {
            let current = t.lock().unwrap_or_else(|p| p.into_inner()).clone();
            let target = current.ok_or_else(|| anyhow::anyhow!("no remote-desktop session yet"))?;
            Ok(EisLink {
                fd: target.eis_fd()?,
                label: format!("session {}", target.label()),
                // Mutter closes the EIS context with the session.
                on_close: None,
            })
        };
        let inner = input_eis::Input::spawn(CONFIG, String::new(), Box::new(connector))?;
        let input_seen = inner.input_seen.clone();
        Ok(Self {
            inner,
            target,
            input_seen,
        })
    }

    /// Use this session's EIS from now on; `mapping_id` picks its region.
    pub fn retarget(&self, target: EisTarget, mapping_id: Option<String>) {
        *self.target.lock().unwrap_or_else(|p| p.into_inner()) = Some(target);
        self.inner.set_region(mapping_id.unwrap_or_default());
    }

    pub fn inject(&self, ev: InputEvent) {
        self.inner.inject(ev);
    }

    pub fn connect(&self) {
        self.inner.connect();
    }

    pub fn set_size(&self, width: u32, height: u32) {
        self.inner.set_size(width, height);
    }

    pub fn shutdown(&self) {
        self.inner.shutdown();
    }
}
