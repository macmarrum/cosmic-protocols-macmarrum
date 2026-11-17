/*
 * SPDX-FileCopyrightText: 2026 https://github.com/pop-os/cosmic-protocols (Original MIT code)
 * SPDX-FileCopyrightText: 2026 macmarrum (at) outlook (dot) ie (GPL-3.0-additions)
 * SPDX-License-Identifier: GPL-3.0-only
 */

use cosmic_client_toolkit::cosmic_protocols::toplevel_info::v1::client::zcosmic_toplevel_handle_v1;
use cosmic_client_toolkit::toplevel_info::{ToplevelInfoHandler, ToplevelInfoState};
use sctk::{
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
};
use wayland_client::{Connection, QueueHandle, globals::registry_queue_init, protocol::wl_output};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1;

mod keymapper {
    //! Minimal client for keymapper's control IPC port ("keymapperctl"),
    //! byte-compatible with `ClientPort` in keymapper's source.

    use std::env;
    use std::io::{Read, Write};
    use std::os::unix::io::FromRawFd;
    use std::os::unix::net::UnixStream;
    use std::process::Command;
    use std::sync::OnceLock;
    use std::time::Duration;

    /// Socket id keymapper's `Host` is constructed with (ClientPort.cpp).
    const IPC_ID: &str = "keymapperctl";
    /// `MessageType::inject_focused` (common/MessageType.h).
    const MESSAGE_TYPE_INJECT_FOCUSED: u8 = 16;
    /// `MessageType::virtual_key_state`, the reply to a request.
    const MESSAGE_TYPE_VIRTUAL_KEY_STATE: u8 = 5;

    /// Overrides the probed keymapper version (e.g. "5.8.0-5-g1f28410f").
    const ENV_VERSION: &str = "KEYMAPPER_FOCUS_VERSION";
    /// Set to 1 to log the IPC handshake and replies on stderr.
    const ENV_VERBOSE: &str = "KEYMAPPER_FOCUS_VERBOSE";

    /// Versions tried when the banner-parsed one fails: "" covers builds
    /// from release tarballs (CMake generates an empty `_version.h`
    /// without git metadata), the rest are recent release tags, all with
    /// the `inject_focused` message (added in keymapper 5.8.0).
    const FALLBACK_VERSIONS: [&str; 4] = ["", "5.8.0", "5.7.3", "5.7.2"];

    /// Applied to both the handshake and the reply read; generous, because
    /// keymapper sleeps 100ms before closing a version-mismatched
    /// connection, which delays the next probe. It also bounds the reply
    /// read, which must never block the Wayland dispatch loop.
    const READ_TIMEOUT: Duration = Duration::from_millis(500);

    pub struct KeymapperPort {
        /// Version whose hash keymapper accepted, remembered between events.
        version: Option<String>,
    }

    impl KeymapperPort {
        pub fn new() -> Self {
            Self { version: None }
        }

        /// Sends one window focus update (title, app_id).
        /// Errors are ignored on purpose: keymapper may not be running.
        pub fn send_window_focus(&mut self, title: &str, app_id: &str) {
            let q_app_id = quote_value(app_id);
            let q_title = quote_value(title);
            println!("app_id: {q_app_id}, title: {q_title}");
            let payload =
                format!("class={} title={}", q_app_id, q_title);
            self.deliver(&payload);
        }

        fn deliver(&mut self, payload: &str) {
            let mut tried: Vec<String> = Vec::new();

            // Phase 1: remembered version and explicit override. This
            // costs no process spawn and works even when keymapper is not
            // running at all.
            let early = [self.version.clone(), env_override()];
            for version in early.into_iter().flatten() {
                if tried.iter().any(|v| *v == version) {
                    continue;
                }
                match try_send(&version, payload) {
                    Delivery::Sent => {
                        self.version = Some(version);
                        return;
                    }
                    // keymapper is not running: keep the remembered version.
                    Delivery::Absent => return,
                    Delivery::Rejected => tried.push(version),
                }
            }

            // Phase 2: the version banner of an installed keymapper binary.
            if let Some(version) = installed_version() {
                if !tried.iter().any(|v| *v == version) {
                    match try_send(&version, payload) {
                        Delivery::Sent => {
                            self.version = Some(version);
                            return;
                        }
                        Delivery::Absent => return,
                        Delivery::Rejected => tried.push(version),
                    }
                }
            }

            // Phase 3: tarball builds and recent release tags.
            for version in FALLBACK_VERSIONS {
                let version = version.to_string();
                if tried.iter().any(|v| *v == version) {
                    continue;
                }
                match try_send(&version, payload) {
                    Delivery::Sent => {
                        self.version = Some(version);
                        return;
                    }
                    Delivery::Absent => return,
                    Delivery::Rejected => tried.push(version),
                }
            }

            // keymapper runs a version that could not be identified; forget
            // the attempts so the next focus event probes from scratch.
            self.version = None;
        }
    }

    enum Delivery {
        /// Message written to an accepted connection.
        Sent,
        /// keymapper is not running or did not answer; stop for this event.
        Absent,
        /// Handshake rejected the version; try the next candidate.
        Rejected,
    }

    fn try_send(version: &str, payload: &str) -> Delivery {
        let mut stream = match connect_abstract(IPC_ID) {
            Ok(stream) => stream,
            Err(_) => return Delivery::Absent,
        };
        let _ = stream.set_read_timeout(Some(READ_TIMEOUT));

        // Handshake: 8-byte hash of the version, 1-byte reply.
        let hash = libstdcxx_hash(version.as_bytes());
        if stream.write_all(&hash.to_le_bytes()).is_err() {
            return Delivery::Absent;
        }
        let mut accepted = [0u8; 1];
        if stream.read_exact(&mut accepted).is_err() {
            return Delivery::Absent;
        }
        if accepted[0] != 1 {
            if verbose() {
                eprintln!("keymapper-focus: version {version:?} rejected");
            }
            return Delivery::Rejected;
        }

        // Frame: [u32 payload length][u8 message type][u32 string length][string].
        let mut frame = Vec::with_capacity(9 + payload.len());
        let body_len = payload.len() as u32 + 5;
        frame.extend_from_slice(&body_len.to_le_bytes());
        frame.push(MESSAGE_TYPE_INJECT_FOCUSED);
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(payload.as_bytes());
        if stream.write_all(&frame).is_err() {
            return Delivery::Absent;
        }

        // Reply: [u32 5][u8 5][u16 key][u16 state]; state Up (0) is success,
        // Not (2) means keymapper rejected the payload string.
        let _ = read_reply(&mut stream);
        Delivery::Sent
    }

    fn read_reply(stream: &mut UnixStream) -> Option<u16> {
        let mut head = [0u8; 5];
        stream.read_exact(&mut head).ok()?;
        let len = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
        if len != 5 || head[4] != MESSAGE_TYPE_VIRTUAL_KEY_STATE {
            return None;
        }
        let mut rest = [0u8; 4];
        stream.read_exact(&mut rest).ok()?;
        let state = u16::from_le_bytes([rest[2], rest[3]]);
        if verbose() {
            eprintln!("keymapper-focus: reply state {state}");
        }
        Some(state)
    }

    /// Connects to the abstract socket keymapper listens on. The address
    /// length is the full `sizeof(sockaddr_un)`, exactly as keymapper's
    /// `Host::connect`/`listen` pass it, so the abstract name including
    /// its zero padding matches; a name-only address gets refused.
    fn connect_abstract(ipc_id: &str) -> std::io::Result<UnixStream> {
        let name = ipc_id.as_bytes();
        assert!(1 + name.len() < 108, "ipc id too long");
        unsafe {
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut addr: libc::sockaddr_un = std::mem::zeroed();
            addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
            // Abstract namespace: the leading NUL is part of the name.
            addr.sun_path[0] = 0;
            for (i, &b) in name.iter().enumerate() {
                addr.sun_path[1 + i] = b as libc::c_char;
            }
            let len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
            if libc::connect(
                fd,
                &addr as *const libc::sockaddr_un as *const libc::sockaddr,
                len,
            ) != 0
            {
                let err = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(err);
            }
            Ok(UnixStream::from_raw_fd(fd))
        }
    }

    /// Quotes a value for keymapper's attribute parser, which has no
    /// escape sequences: a string ends at its first quote character.
    fn quote_value(value: &str) -> String {
        if !value.contains('\'') {
            format!("'{value}'")
        } else if !value.contains('"') {
            format!("\"{value}\"")
        } else {
            // Contains both quote characters, which cannot be represented
            // losslessly; drop the single quotes to stay parseable.
            format!("'{}'", value.replace('\'', ""))
        }
    }

    fn env_override() -> Option<String> {
        env::var(ENV_VERSION).ok().filter(|v| !v.is_empty())
    }

    fn verbose() -> bool {
        static VERBOSE: OnceLock<bool> = OnceLock::new();
        *VERBOSE.get_or_init(|| {
            matches!(env::var(ENV_VERBOSE).as_deref(), Ok("1") | Ok("true") | Ok("TRUE"))
        })
    }

    /// Version from the first line of an installed binary's --help banner
    /// ("keymapperctl 5.8.0"), which is the exact string keymapper hashes.
    /// Computed once; all binaries of one build share the version.
    fn installed_version() -> Option<String> {
        static VERSION: OnceLock<Option<String>> = OnceLock::new();
        VERSION
            .get_or_init(|| {
                ["keymapperctl", "keymapperd", "keymapper"]
                    .iter()
                    .find_map(|cmd| version_from_help(cmd))
            })
            .clone()
    }

    fn version_from_help(cmd: &str) -> Option<String> {
        let output = Command::new(cmd).arg("--help").output().ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        text.lines().find_map(|line| {
            let rest = line.strip_prefix(cmd)?.trim_start();
            let token = rest.split_whitespace().next()?;
            is_version_like(token).then(|| token.to_string())
        })
    }

    fn is_version_like(s: &str) -> bool {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'))
    }

    /// Exact port of libstdc++'s `_Hash_bytes` (64-bit `std::hash`, seed
    /// `0xc70f6907`), which keymapper's `Host::accept` compares against.
    /// The 8-byte loads are native, i.e. little-endian on all supported
    /// targets; keymapper versions are ASCII.
    fn libstdcxx_hash(bytes: &[u8]) -> u64 {
        const SEED: u64 = 0xc70f_6907;
        const MUL: u64 = 0xc6a4_a793_5bd1_e995;

        fn shift_mix(v: u64) -> u64 {
            v ^ (v >> 47)
        }

        let mut hash = SEED ^ (bytes.len() as u64).wrapping_mul(MUL);
        let mut chunks = bytes.chunks_exact(8);
        for chunk in chunks.by_ref() {
            let mut word = [0u8; 8];
            word.copy_from_slice(chunk);
            let data =
                shift_mix(u64::from_le_bytes(word).wrapping_mul(MUL)).wrapping_mul(MUL);
            hash ^= data;
            hash = hash.wrapping_mul(MUL);
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            // `_Hash_impl::load_bytes`: fold from the last byte to the first.
            let mut data: u64 = 0;
            for &b in rest.iter().rev() {
                data = (data << 8) | b as u64;
            }
            hash ^= data;
            hash = hash.wrapping_mul(MUL);
        }
        shift_mix(shift_mix(hash).wrapping_mul(MUL))
    }
}

struct AppData {
    output_state: OutputState,
    registry_state: RegistryState,
    toplevel_info_state: ToplevelInfoState,
    /// (title, app_id) last sent to keymapper, so we only send on change.
    last_focus: Option<(String, String)>,
    /// Remembers which keymapper version the IPC handshake accepted.
    keymapper: keymapper::KeymapperPort,
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
        self.keymapper.send_window_focus(&focused.0, &focused.1);
        self.last_focus = Some(focused);
    }
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
        keymapper: keymapper::KeymapperPort::new(),
    };

    loop {
        event_queue.blocking_dispatch(&mut app_data).unwrap();
    }
}

sctk::delegate_output!(AppData);
sctk::delegate_registry!(AppData);
cosmic_client_toolkit::delegate_toplevel_info!(AppData);
