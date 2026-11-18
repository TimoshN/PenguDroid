//! Maliit input-method integration over its peer-to-peer D-Bus socket.
//!
//! The connection and input context live for the daemon lifetime. Text contents
//! are deliberately not logged: they can contain passwords or PIN codes.

use std::collections::HashMap;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

use zbus::blocking::{connection::Builder as ConnectionBuilder, Proxy};
use zbus::zvariant::Value;

const SERVER_PATH: &str = "/com/meego/inputmethod/uiserver1";
const SERVER_INTERFACE: &str = "com.meego.inputmethod.uiserver1";
const CONTEXT_PATH: &str = "/com/meego/inputmethod/inputcontext";

pub enum ImeEvent {
    Commit(String),
    Preedit(String),
    Key { key: i32, text: String },
    FinishComposing,
}

struct InputContext {
    hidden: Arc<AtomicBool>,
    events: std::sync::mpsc::Sender<ImeEvent>,
}

#[zbus::interface(name = "com.meego.inputmethod.inputcontext1")]
impl InputContext {
    #[zbus(name = "commitString")]
    fn commit_string(&self, string: String, replace_start: i32, replace_length: i32, cursor_pos: i32) {
        let _ = (replace_start, replace_length, cursor_pos);
        let _ = self.events.send(ImeEvent::Commit(string));
    }

    #[zbus(name = "updatePreedit")]
    fn update_preedit(
        &self,
        string: String,
        format_list: zbus::zvariant::OwnedValue,
        replace_start: i32,
        replace_length: i32,
        cursor_pos: i32,
    ) {
        let _ = (format_list, replace_start, replace_length, cursor_pos);
        let _ = self.events.send(ImeEvent::Preedit(string));
    }

    #[zbus(name = "keyEvent")]
    #[allow(clippy::too_many_arguments)]
    fn key_event(
        &self,
        event_type: i32,
        key: i32,
        modifiers: i32,
        text: String,
        auto_repeat: bool,
        count: i32,
        request_type: u8,
    ) {
        let _ = (modifiers, auto_repeat, count, request_type);
        if event_type == 6 { // QEvent::KeyPress; do not commit release twice.
            let _ = self.events.send(ImeEvent::Key { key, text });
        }
    }

    #[zbus(name = "imInitiatedHide")]
    fn im_initiated_hide(&self) {
        self.hidden.store(true, Ordering::Release);
        let _ = self.events.send(ImeEvent::FinishComposing);
    }

    #[zbus(name = "activationLostEvent")]
    fn activation_lost_event(&self) {
        self.hidden.store(true, Ordering::Release);
        let _ = self.events.send(ImeEvent::FinishComposing);
    }
}

pub struct Maliit {
    proxy: Proxy<'static>,
    visible: bool,
    hidden: Arc<AtomicBool>,
    events: std::sync::mpsc::Receiver<ImeEvent>,
}

impl Maliit {
    pub fn connect() -> Result<Self, String> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .ok_or("XDG_RUNTIME_DIR is unset")?;
        let address = format!(
            "unix:path={}/maliit-server/dbus-socket",
            std::path::PathBuf::from(runtime).display(),
        );
        let hidden = Arc::new(AtomicBool::new(false));
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let connection = ConnectionBuilder::address(address.as_str())
            .map_err(|e| format!("invalid Maliit address: {e}"))?
            .p2p()
            .serve_at(CONTEXT_PATH, InputContext {
                hidden: Arc::clone(&hidden),
                events: event_tx,
            })
            .map_err(|e| format!("cannot register Maliit input context: {e}"))?
            .build()
            .map_err(|e| format!("cannot connect to Maliit: {e}"))?;
        let proxy = Proxy::new(&connection, "org.maliit.server", SERVER_PATH, SERVER_INTERFACE)
            .map_err(|e| format!("cannot create Maliit proxy: {e}"))?;
        Ok(Self { proxy, visible: false, hidden, events: event_rx })
    }

    pub fn drain_events(&self) -> Vec<ImeEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            events.push(event);
        }
        events
    }

    pub fn set_visible(&mut self, visible: bool) -> Result<(), String> {
        if visible == self.visible && !(visible && self.hidden.swap(false, Ordering::AcqRel)) {
            return Ok(());
        }
        if visible {
            let _: () = self.proxy.call("activateContext", &())
                .map_err(|e| format!("activateContext: {e}"))?;
            self.send_orientation(0)?;
            let mut state = HashMap::new();
            state.insert("focusState", Value::from(true));
            state.insert("contentType", Value::from(0i32));
            state.insert("predictionEnabled", Value::from(false));
            state.insert("correctionEnabled", Value::from(false));
            let _: () = self.proxy.call("updateWidgetInformation", &(state, true))
                .map_err(|e| format!("updateWidgetInformation: {e}"))?;
            let _: () = self.proxy.call("showInputMethod", &())
                .map_err(|e| format!("showInputMethod: {e}"))?;
            self.send_orientation(0)?;
        } else {
            let _: () = self.proxy.call("hideInputMethod", &())
                .map_err(|e| format!("hideInputMethod: {e}"))?;
        }
        self.visible = visible;
        self.hidden.store(false, Ordering::Release);
        Ok(())
    }

    fn send_orientation(&self, angle: i32) -> Result<(), String> {
        let _: () = self.proxy.call("appOrientationAboutToChange", &(angle,))
            .map_err(|e| format!("appOrientationAboutToChange: {e}"))?;
        let _: () = self.proxy.call("appOrientationChanged", &(angle,))
            .map_err(|e| format!("appOrientationChanged: {e}"))?;
        Ok(())
    }
}
