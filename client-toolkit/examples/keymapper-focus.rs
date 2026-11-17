use std::process::{Command, Stdio};

use cosmic_client_toolkit::cosmic_protocols::toplevel_info::v1::client::zcosmic_toplevel_handle_v1;
use cosmic_client_toolkit::toplevel_info::{ToplevelInfoHandler, ToplevelInfoState};
use sctk::{
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
};
use wayland_client::{Connection, QueueHandle, globals::registry_queue_init, protocol::wl_output};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1;

struct AppData {
    output_state: OutputState,
    registry_state: RegistryState,
    toplevel_info_state: ToplevelInfoState,
    /// (title, app_id) last sent to keymapper, so we only send on change.
    last_focus: Option<(String, String)>,
}

impl AppData {
    fn report_focus(&mut self) {
        // Copy the values out so the borrow of toplevel_info_state ends here.
        let focused: (String, String) = self
            .toplevel_info_state
            .toplevels()
            .find(|info| info.state.contains(&zcosmic_toplevel_handle_v1::State::Activated))
            .map(|info| (info.title.clone(), info.app_id.clone()))
            .unwrap_or_default();

        if self.last_focus.as_ref() == Some(&focused) {
            return;
        }
        send_window_focus(&focused.0, &focused.1);
        self.last_focus = Some(focused);
    }
}

fn send_window_focus(title: &str, app_id: &str) {
    let title = gvariant_string(title);
    let app_id = gvariant_string(app_id);
    println!("app_id: {}, title: {}", app_id, title);

    // Errors are ignored on purpose: keymapper may not be running.
    let _ = Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "com.github.houmain.Keymapper",
            "--object-path",
            "/com/github/houmain/Keymapper",
            "--method",
            "com.github.houmain.Keymapper.WindowFocus",
            title.as_str(),
            app_id.as_str(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Quote a string as a GVariant text-format string literal, which gdbus parses.
fn gvariant_string(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    format!("\"{escaped}\"")
}

impl ProvidesRegistryState for AppData {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    sctk::registry_handlers!(OutputState);
}

// Needed so the toplevel protocol gets output events.
impl OutputHandler for AppData {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ToplevelInfoHandler for AppData {
    fn toplevel_info_state(&mut self) -> &mut ToplevelInfoState {
        &mut self.toplevel_info_state
    }

    // Focus is checked once per batch in info_done, so these are not needed.
    fn new_toplevel(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    ) {
    }

    fn update_toplevel(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    ) {
    }

    fn toplevel_closed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    ) {
    }

    // Sent after each batch of toplevel changes, so the state is consistent here.
    fn info_done(&mut self, _: &Connection, _: &QueueHandle<Self>) {
        self.report_focus();
    }
}

fn main() {
    let conn = Connection::connect_to_env().unwrap();
    let (globals, mut event_queue) = registry_queue_init(&conn).unwrap();
    let qh = event_queue.handle();

    let registry_state = RegistryState::new(&globals);
    let mut app_data = AppData {
        output_state: OutputState::new(&globals, &qh),
        toplevel_info_state: ToplevelInfoState::new(&registry_state, &qh),
        registry_state,
        last_focus: None,
    };

    loop {
        event_queue.blocking_dispatch(&mut app_data).unwrap();
    }
}

sctk::delegate_output!(AppData);
sctk::delegate_registry!(AppData);
cosmic_client_toolkit::delegate_toplevel_info!(AppData);
