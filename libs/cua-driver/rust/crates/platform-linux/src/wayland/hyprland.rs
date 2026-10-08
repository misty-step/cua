//! Attested Hyprland identity, capture and explicit foreground activation.
//!
//! Native IDs are full compositor addresses, never title matches or truncated
//! protocol object IDs. IPC, capture and focus must name the same compositor.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

const QUERY_TIMEOUT: Duration = Duration::from_secs(1);
const QUERY_TOTAL_TIMEOUT: Duration = Duration::from_secs(3);
const QUERY_RETRY_BACKOFF: Duration = Duration::from_millis(50);
const QUERY_MAX_ATTEMPTS: usize = 2;
const MAX_REPLY_BYTES: usize = 4 * 1024 * 1024;
const MAX_LOGICAL_PIXELS: u64 = 64 * 1024 * 1024;
const MAX_INSTANCE_ENTRIES: usize = 32;
const FOCUS_TIMEOUT: Duration = Duration::from_millis(500);
const FOCUS_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct Workspace {
    id: i64,
}

#[derive(Clone, Debug, Deserialize)]
struct Client {
    address: String,
    mapped: bool,
    hidden: bool,
    pid: i64,
    title: String,
    class: String,
    at: [i32; 2],
    size: [i32; 2],
    workspace: Workspace,
}

#[derive(Deserialize)]
struct Monitor {
    #[serde(rename = "activeWorkspace")]
    active_workspace: Workspace,
    #[serde(rename = "specialWorkspace")]
    special_workspace: Workspace,
}

#[derive(Clone, Debug, Deserialize)]
struct DisplayMonitor {
    width: u32,
    height: u32,
    scale: f64,
    x: i32,
    y: i32,
    transform: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Window {
    pub address: u64,
    pub pid: u32,
    pub title: String,
    pub app_id: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub workspace: i64,
    pub visible: bool,
    hidden: bool,
}

struct Session {
    ipc_path: PathBuf,
    wayland_path: PathBuf,
    peer: OnceLock<libc::pid_t>,
}

enum SessionSelection {
    Absent,
    Ready(Session),
    Refused(String),
}

// The driver session environment is immutable. Discovery happens once, never
// on a capture/input hot path or again after an identity failure.
static SESSION: LazyLock<SessionSelection> = LazyLock::new(resolve_session);

fn resolve_session() -> SessionSelection {
    if super::is_inject_mode() {
        return SessionSelection::Absent;
    }
    let signature = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE");
    if signature.is_none() && !super::wayland_input_enabled() {
        return SessionSelection::Absent;
    }
    let resolved = (|| -> Result<Option<Session>> {
        let runtime =
            PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").context("missing XDG_RUNTIME_DIR")?);
        if !runtime.is_absolute() {
            bail!("XDG_RUNTIME_DIR must be absolute");
        }
        let wayland_path = named_wayland_path(&runtime)?;
        if let Some(signature) = signature {
            let signature = signature
                .to_str()
                .context("invalid Hyprland instance signature")?;
            if !valid_signature(signature) {
                bail!("invalid Hyprland instance signature");
            }
            // An explicit signature, including a stale or invalid one, is
            // authoritative: never scan for a replacement desktop.
            return Ok(Some(Session {
                ipc_path: runtime.join("hypr").join(signature).join(".socket.sock"),
                wayland_path,
                peer: OnceLock::new(),
            }));
        }
        discover_session(&runtime, wayland_path)
    })();
    match resolved {
        Ok(Some(session)) => SessionSelection::Ready(session),
        Ok(None) => SessionSelection::Absent,
        Err(error) => SessionSelection::Refused(format!("{error:#}")),
    }
}

fn session_selection() -> &'static SessionSelection {
    &SESSION
}

pub fn is_session() -> bool {
    !matches!(session_selection(), SessionSelection::Absent)
}

fn session() -> Result<&'static Session> {
    match session_selection() {
        SessionSelection::Ready(session) => Ok(session),
        SessionSelection::Absent => bail!("no attested Hyprland instance for this Wayland display"),
        SessionSelection::Refused(error) => bail!("Hyprland session refused: {error}"),
    }
}

fn valid_signature(signature: &str) -> bool {
    !signature.is_empty()
        && signature != "."
        && signature != ".."
        && signature
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

fn named_wayland_path(runtime: &Path) -> Result<PathBuf> {
    if std::env::var_os("WAYLAND_SOCKET").is_some() {
        bail!("Hyprland requires a named WAYLAND_DISPLAY socket");
    }
    let display =
        PathBuf::from(std::env::var_os("WAYLAND_DISPLAY").context("missing WAYLAND_DISPLAY")?);
    if display.is_absolute() {
        return Ok(display);
    }
    if display.components().count() != 1
        || !matches!(
            display.components().next(),
            Some(std::path::Component::Normal(_))
        )
    {
        bail!("invalid WAYLAND_DISPLAY socket name");
    }
    Ok(runtime.join(display))
}

fn socket_connection(path: &Path, timeout: Duration) -> Result<UnixStream> {
    let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
    socket.connect_timeout(&socket2::SockAddr::unix(path)?, timeout)?;
    Ok(socket.into())
}

fn discover_session(runtime: &Path, wayland_path: PathBuf) -> Result<Option<Session>> {
    let entries = match std::fs::read_dir(runtime.join("hypr")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Hyprland instance directory unavailable"),
    };
    let deadline = Instant::now() + QUERY_TIMEOUT;
    let wayland = socket_connection(&wayland_path, query_time_remaining(deadline)?)?;
    let compositor = peer_pid(wayland.as_raw_fd())?;
    let mut selected = None;
    for (index, entry) in entries.enumerate() {
        if index >= MAX_INSTANCE_ENTRIES {
            bail!("Hyprland instance discovery exceeded its bounded directory limit");
        }
        query_time_remaining(deadline)?;
        let entry = entry?;
        if !entry.file_type()?.is_dir() || !entry.file_name().to_str().is_some_and(valid_signature)
        {
            continue;
        }
        let ipc_path = entry.path().join(".socket.sock");
        let ipc = match socket_connection(&ipc_path, query_time_remaining(deadline)?) {
            Ok(ipc) => ipc,
            Err(_) => continue,
        };
        if peer_pid(ipc.as_raw_fd()).ok() != Some(compositor) {
            continue;
        }
        if selected.is_some() {
            bail!("multiple Hyprland IPC instances attest to the named Wayland compositor");
        }
        selected = Some(ipc_path);
    }
    Ok(selected.map(|ipc_path| Session {
        ipc_path,
        wayland_path,
        peer: OnceLock::from(compositor),
    }))
}

fn verify_session_peer(peer: libc::pid_t) -> Result<()> {
    let expected = *session()?.peer.get_or_init(|| peer);
    if peer != expected {
        bail!("Hyprland compositor changed during the driver session");
    }
    Ok(())
}

pub(super) fn peer_pid(fd: RawFd) -> Result<libc::pid_t> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if result != 0
        || len as usize != std::mem::size_of::<libc::ucred>()
        || cred.pid <= 0
        || cred.uid != unsafe { libc::geteuid() }
    {
        bail!("could not verify same-user compositor peer");
    }
    Ok(cred.pid)
}

fn ipc_connection() -> Result<UnixStream> {
    ipc_connection_with_timeout(QUERY_TIMEOUT)
}

fn ipc_connection_with_timeout(timeout: Duration) -> Result<UnixStream> {
    socket_connection(&session()?.ipc_path, timeout)
}

/// Bound the socket connection as well as protocol dispatch. Inherited
/// WAYLAND_SOCKET descriptors are deliberately unsupported here: this adapter
/// opens independent, attested connections for each observation.
pub(super) fn wayland_connection() -> Result<wayland_client::Connection> {
    wayland_connection_with_timeout(QUERY_TIMEOUT)
}

fn wayland_connection_with_timeout(timeout: Duration) -> Result<wayland_client::Connection> {
    let socket = socket_connection(&session()?.wayland_path, timeout)?;
    verify_session_peer(peer_pid(socket.as_raw_fd())?)?;
    wayland_client::Connection::from_socket(socket).context("Wayland connection failed")
}

/// Bind the Wayland connection to the IPC compositor before accepting pixels.
pub(super) fn verify_capture_peer(connection: &wayland_client::Connection) -> Result<()> {
    let ipc = ipc_connection()?;
    let peer = peer_pid(ipc.as_raw_fd())?;
    if peer != peer_pid(connection.backend().poll_fd().as_raw_fd())? {
        bail!("Hyprland IPC and WAYLAND_DISPLAY name different compositor processes");
    }
    verify_session_peer(peer)?;
    Ok(())
}

fn attested_ipc_connection(deadline: Instant) -> Result<UnixStream> {
    let ipc = ipc_connection_with_timeout(query_time_remaining(deadline)?)?;
    // A stale inherited signature must never name a different nested desktop.
    // Pin the peer for the entire driver session, including focus and restore.
    let wayland = wayland_connection_with_timeout(query_time_remaining(deadline)?)?;
    let peer = peer_pid(ipc.as_raw_fd())?;
    if peer != peer_pid(wayland.backend().poll_fd().as_raw_fd())? {
        bail!("Hyprland IPC and WAYLAND_DISPLAY name different compositor processes");
    }
    verify_session_peer(peer)?;
    Ok(ipc)
}

fn query<T: serde::de::DeserializeOwned>(command: &str) -> Result<T> {
    query_before(command, Instant::now() + QUERY_TOTAL_TIMEOUT)
}

fn query_before<T: serde::de::DeserializeOwned>(command: &str, deadline: Instant) -> Result<T> {
    query_with(
        command,
        QUERY_TIMEOUT,
        query_time_remaining(deadline)?,
        QUERY_RETRY_BACKOFF,
        attested_ipc_connection,
    )
}

fn query_time_remaining(deadline: Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(io::Error::new(io::ErrorKind::TimedOut, "Hyprland IPC query timed out").into());
    }
    Ok(remaining)
}

fn is_query_timeout(error: &anyhow::Error) -> bool {
    error.downcast_ref::<io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        )
    })
}

/// Retry only a timed-out observation, never an action or an identity failure.
/// Each attempt discards all previous bytes and opens newly attested sockets.
fn query_with<T: serde::de::DeserializeOwned>(
    command: &str,
    per_attempt: Duration,
    total: Duration,
    backoff: Duration,
    mut connect: impl FnMut(Instant) -> Result<UnixStream>,
) -> Result<T> {
    let deadline = Instant::now() + total;
    // Keep this a closed list: a generic "j/" prefix also admits JSON-formatted
    // dispatch commands. JSON output does not imply a read-only operation.
    let read_only = matches!(command, "j/monitors" | "j/clients" | "j/activewindow");
    for attempt in 1..=QUERY_MAX_ATTEMPTS {
        query_time_remaining(deadline)?;
        let attempt_deadline = deadline.min(Instant::now() + per_attempt);
        // Connection and peer-attestation errors are permanent. Only read-only
        // request/reply timeouts below are eligible for a new query.
        let mut ipc = connect(attempt_deadline)?;
        let reply = query_time_remaining(attempt_deadline)
            .and_then(|remaining| read_reply(&mut ipc, command.as_bytes(), remaining));
        match reply {
            Ok(bytes) => {
                return serde_json::from_slice(&bytes).context("invalid Hyprland IPC JSON")
            }
            Err(error) => {
                if !read_only
                    || !is_query_timeout(&error)
                    || attempt == QUERY_MAX_ATTEMPTS
                    || deadline.saturating_duration_since(Instant::now()) <= backoff
                {
                    return Err(error).with_context(|| {
                        format!("Hyprland IPC observation failed after {attempt} attempt(s)")
                    });
                }
                drop(ipc);
                tracing::warn!(
                    command,
                    attempt,
                    "Hyprland IPC observation timed out; retrying on a fresh connection"
                );
                std::thread::sleep(backoff);
            }
        }
    }
    unreachable!("the final query attempt always returns")
}

fn read_reply(stream: &mut UnixStream, command: &[u8], timeout: Duration) -> Result<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(command)?;
    let mut reply = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let remaining = query_time_remaining(deadline)?;
        stream.set_read_timeout(Some(remaining))?;
        let count = stream
            .read(&mut chunk)
            .context("Hyprland IPC reply unavailable")?;
        if count == 0 {
            break;
        }
        if reply.len() + count > MAX_REPLY_BYTES {
            bail!("Hyprland IPC reply exceeds size limit");
        }
        reply.extend_from_slice(&chunk[..count]);
    }
    Ok(reply)
}

fn windows_from_clients(clients: Vec<Client>, active: &HashSet<i64>) -> Result<Vec<Window>> {
    let mut seen = HashSet::new();
    let mut windows = Vec::new();
    for c in clients.into_iter().filter(|c| c.mapped) {
        let address = u64::from_str_radix(c.address.strip_prefix("0x").unwrap_or(&c.address), 16)?;
        let pid = u32::try_from(c.pid)?;
        let width = u32::try_from(c.size[0])?;
        let height = u32::try_from(c.size[1])?;
        if address == 0 || pid == 0 || !valid_dimensions(width, height) || !seen.insert(address) {
            bail!("invalid or duplicate Hyprland window identity/geometry");
        }
        windows.push(Window {
            address,
            pid,
            title: c.title,
            app_id: c.class,
            x: c.at[0],
            y: c.at[1],
            width,
            height,
            workspace: c.workspace.id,
            visible: !c.hidden && active.contains(&c.workspace.id),
            hidden: c.hidden,
        });
    }
    Ok(windows)
}

fn valid_dimensions(width: u32, height: u32) -> bool {
    width > 0 && height > 0 && u64::from(width) * u64::from(height) <= MAX_LOGICAL_PIXELS
}

/// Content-free geometry for the qualified single-output, 1:1 desktop.
/// The common policy adapter uses this for display-scoped observation. Never
/// substitute a screenshot, XWayland root, or guessed primary monitor here.
pub fn screen_size() -> Result<(u32, u32, f64)> {
    screen_size_from_monitors(query("j/monitors")?)
}

fn screen_size_from_monitors(monitors: Vec<DisplayMonitor>) -> Result<(u32, u32, f64)> {
    let [monitor] = monitors.as_slice() else {
        bail!("Hyprland display identity requires exactly one active output");
    };
    if monitor.scale != 1.0 || monitor.transform != 0 || monitor.x != 0 || monitor.y != 0 {
        bail!("Hyprland display identity requires an unscaled, unrotated output at the origin");
    }
    if !valid_dimensions(monitor.width, monitor.height) {
        bail!("invalid Hyprland display dimensions");
    }
    Ok((monitor.width, monitor.height, monitor.scale))
}

pub fn list_windows() -> Result<Vec<Window>> {
    let monitors: Vec<Monitor> = query("j/monitors")?;
    let active = monitors
        .into_iter()
        .flat_map(|m| [m.active_workspace.id, m.special_workspace.id])
        .filter(|id| *id != 0)
        .collect();
    windows_from_clients(query("j/clients")?, &active)
}

pub fn window_for_address(address: u64) -> Option<Window> {
    list_windows()
        .ok()?
        .into_iter()
        .find(|w| w.address == address)
}

/// AT-SPI has no native Hyprland handle. Correlate only when the title is
/// unique among this PID's mapped compositor clients, as well as AX roots.
pub fn accessibility_window(address: u64, pid: u32) -> Option<Window> {
    accessibility_target(&list_windows().ok()?, address, pid)
}

fn accessibility_target(windows: &[Window], address: u64, pid: u32) -> Option<Window> {
    let target = windows
        .iter()
        .find(|w| w.address == address && w.pid == pid)?;
    (!target.title.is_empty()
        && windows
            .iter()
            .filter(|w| w.pid == pid && w.title == target.title)
            .count()
            == 1)
        .then(|| target.clone())
}

/// The legacy PID-only bounds caller has no window identity: allow only a
/// single mapped client. Explicit IDs never fall back to this function.
pub fn window_for_pid(pid: u32) -> Option<Window> {
    let mut owned = list_windows().ok()?.into_iter().filter(|w| w.pid == pid);
    let first = owned.next()?;
    owned.next().is_none().then_some(first)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TargetIdentity {
    address: u64,
    pid: u32,
}

fn focus_target(windows: &[Window], target: TargetIdentity) -> Result<()> {
    let window = windows
        .iter()
        .find(|window| window.address == target.address)
        .context("foreground_unavailable: exact Hyprland window no longer exists")?;
    if window.pid != target.pid || window.hidden {
        bail!("foreground_unavailable: exact Hyprland target ownership/visibility is unproven");
    }
    Ok(())
}

fn focus_from_active(active: &serde_json::Value) -> Result<Option<TargetIdentity>> {
    let object = active
        .as_object()
        .context("invalid Hyprland activewindow reply")?;
    if object.is_empty() {
        return Ok(None);
    }
    let address = active
        .get("address")
        .and_then(|value| value.as_str())
        .and_then(|address| address.strip_prefix("0x"))
        .context("invalid Hyprland active window address")?;
    let address = u64::from_str_radix(address, 16)?;
    let pid = u32::try_from(
        active
            .get("pid")
            .and_then(|value| value.as_u64())
            .context("invalid Hyprland active window PID")?,
    )?;
    if address == 0 || pid == 0 {
        bail!("invalid Hyprland active window identity");
    }
    Ok(Some(TargetIdentity { address, pid }))
}

fn active_target(deadline: Instant) -> Result<Option<TargetIdentity>> {
    focus_from_active(&query_before("j/activewindow", deadline)?)
}

fn verify_live_target(target: TargetIdentity, deadline: Instant) -> Result<()> {
    let windows = windows_from_clients(query_before("j/clients", deadline)?, &HashSet::new())?;
    focus_target(&windows, target)
}

fn prior_focus(deadline: Instant) -> Result<Option<TargetIdentity>> {
    let prior = active_target(deadline)?;
    if let Some(prior) = prior {
        verify_live_target(prior, deadline)?;
    }
    Ok(prior)
}

/// Recheck after child AX focus or any readiness wait, without reactivating a
/// lost target. A changed focus refuses; no uncertain input is replayed.
pub fn require_target_focus(address: u64, pid: u32) -> Result<()> {
    let target = TargetIdentity { address, pid };
    let deadline = Instant::now() + QUERY_TOTAL_TIMEOUT;
    verify_live_target(target, deadline)?;
    if active_target(deadline)? != Some(target) {
        bail!("foreground_unavailable: Hyprland focus changed; no keyboard input was sent");
    }
    Ok(())
}

fn focus_once(target: TargetIdentity, deadline: Instant) -> Result<()> {
    // Qualified stock Hyprland's Lua dispatcher, using only a numeric full
    // address. Never try a title selector or another mutation after uncertainty.
    let command = format!(
        "/dispatch hl.dsp.focus({{ window = \"address:0x{:x}\" }})",
        target.address
    );
    let mut ipc = attested_ipc_connection(deadline)?;
    let reply = read_reply(
        &mut ipc,
        command.as_bytes(),
        query_time_remaining(deadline)?,
    )?;
    if std::str::from_utf8(&reply)?.trim() != "ok" {
        bail!("foreground_unavailable: Hyprland did not acknowledge exact-address focus");
    }
    Ok(())
}

fn switch_focus(target: TargetIdentity, expected: Option<TargetIdentity>) -> Result<()> {
    let deadline = Instant::now() + FOCUS_TIMEOUT;
    verify_live_target(target, deadline)?;
    if active_target(deadline)? != expected {
        bail!("foreground_unavailable: Hyprland focus changed before activation");
    }
    focus_once(target, deadline)?;
    loop {
        let active = active_target(deadline)?;
        if active == Some(target) {
            return Ok(());
        }
        if active != expected {
            bail!("foreground_unavailable: independent Hyprland focus change during activation");
        }
        std::thread::sleep(FOCUS_POLL_INTERVAL.min(query_time_remaining(deadline)?));
    }
}

fn restore_prior(target: TargetIdentity, prior: Option<TargetIdentity>) -> Result<()> {
    let deadline = Instant::now() + QUERY_TOTAL_TIMEOUT;
    if active_target(deadline)? != Some(target) {
        bail!("Hyprland focus changed independently; prior focus was not overwritten");
    }
    verify_live_target(target, deadline)?;
    if let Some(prior) = prior.filter(|prior| *prior != target) {
        verify_live_target(prior, deadline)?;
        switch_focus(prior, Some(target))?;
        require_target_focus(prior.address, prior.pid)?;
    }
    Ok(())
}

fn finish_focus<T>(result: Result<T>, restore: Result<()>) -> Result<T> {
    match (result, restore) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(anyhow::anyhow!(
            "foreground_effect_unknown: the foreground operation returned without application-effect verification; final focus/restore failed: {error:#}"
        )),
        (Err(error), Err(restore)) => Err(anyhow::anyhow!(
            "Hyprland foreground operation failed: {error:#}; prior focus also could not be restored: {restore:#}"
        )),
    }
}

static FOCUS_TRANSACTION: Mutex<()> = Mutex::new(());

/// Explicit foreground activation leaves the exact target focused on success.
pub fn activate_window(address: u64, pid: u32) -> Result<()> {
    let _transaction = FOCUS_TRANSACTION
        .lock()
        .map_err(|_| anyhow::anyhow!("Hyprland foreground transaction lock poisoned"))?;
    let target = TargetIdentity { address, pid };
    let deadline = Instant::now() + QUERY_TOTAL_TIMEOUT;
    verify_live_target(target, deadline)?;
    let prior = prior_focus(deadline)?;
    let result = if prior == Some(target) {
        require_target_focus(address, pid)
    } else {
        switch_focus(target, prior).and_then(|()| require_target_focus(address, pid))
    };
    match result {
        Ok(()) => Ok(()),
        Err(error) => finish_focus(Err(error), restore_prior(target, prior)),
    }
}

/// Focus/readback permits opt-in foreground virtual-keyboard input; it is NOT
/// compositor-atomic binding. A user/compositor can change focus between the
/// final check and an event during this short action. The mutex serializes only
/// driver transactions. Application effect remains unverifiable until observed.
pub fn with_focused_window<T>(
    pid: u32,
    address: u64,
    body: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let _transaction = FOCUS_TRANSACTION
        .lock()
        .map_err(|_| anyhow::anyhow!("Hyprland foreground transaction lock poisoned"))?;
    let target = TargetIdentity { address, pid };
    let deadline = Instant::now() + QUERY_TOTAL_TIMEOUT;
    verify_live_target(target, deadline)?;
    let prior = prior_focus(deadline)?;
    let result = (|| {
        if prior != Some(target) {
            switch_focus(target, prior)?;
        }
        require_target_focus(address, pid)?;
        body()
    })();
    // Restore after operation/activation failures too, but never over an
    // independently observed focus change or a dead/stale prior identity.
    finish_focus(result, restore_prior(target, prior))
}

fn capture_target(windows: &[Window], address: u64, pid: Option<u32>) -> Result<Window> {
    let target = windows
        .iter()
        .find(|w| w.address == address)
        .context("requested Hyprland window no longer exists; refresh list_windows")?;
    if pid.is_some_and(|pid| target.pid != pid) || target.hidden {
        bail!("requested Hyprland target ownership/visibility is unproven");
    }
    // The v1 wire request takes only the low word. Refuse collisions across
    // ALL mapped clients, including other processes and hidden windows.
    if windows
        .iter()
        .filter(|w| w.address as u32 == address as u32)
        .count()
        != 1
    {
        bail!("Hyprland toplevel export handle is ambiguous");
    }
    Ok(target.clone())
}

pub fn capture(address: u64, pid: Option<u32>) -> Result<Vec<u8>> {
    let before = capture_target(&list_windows()?, address, pid)?;
    let bytes = super::hyprland_capture::capture_toplevel_png(address)?;
    let after = capture_target(&list_windows()?, address, pid)?;
    if before != after {
        bail!("Hyprland target changed during capture; refresh the snapshot");
    }
    // Keep the established window-local logical coordinate contract. Physical
    // toplevel buffers can use a fractional render scale; do not make callers
    // infer it from a monitor mode or apply an output-origin offset.
    let image = image::load_from_memory(&bytes)?;
    if image.width() == before.width && image.height() == before.height {
        return Ok(bytes);
    }
    let image = image.resize_exact(
        before.width,
        before.height,
        image::imageops::FilterType::Triangle,
    );
    let mut out = std::io::Cursor::new(Vec::new());
    image.write_to(&mut out, image::ImageFormat::Png)?;
    Ok(out.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(address: u64, pid: u32) -> Window {
        Window {
            address,
            pid,
            title: "Fixture".into(),
            app_id: "fixture".into(),
            x: 967,
            y: 38,
            width: 800,
            height: 600,
            workspace: 1,
            visible: true,
            hidden: false,
        }
    }

    #[test]
    fn exact_identity_never_falls_back_to_sibling_or_other_pid() {
        let windows = [window(0x10, 42), window(0x20, 42)];
        assert_eq!(
            capture_target(&windows, 0x10, Some(42)).unwrap().address,
            0x10
        );
        assert!(capture_target(&windows, 0x30, Some(42)).is_err());
        assert!(capture_target(&windows, 0x10, Some(43)).is_err());
    }

    #[test]
    fn foreground_identity_requires_full_live_address_and_exact_pid() {
        let windows = [window(0x1ff000005, 42), window(0x2ff000005, 42)];
        let target = TargetIdentity {
            address: 0x1ff000005,
            pid: 42,
        };
        assert!(focus_target(&windows, target).is_ok());
        // A connection-local foreign handle cannot borrow a matching low word,
        // nor can a stale ID borrow a same-PID sibling's native authority.
        assert!(focus_target(
            &windows,
            TargetIdentity {
                address: 0xff000005,
                ..target
            },
        )
        .is_err());
        assert!(focus_target(
            &windows,
            TargetIdentity {
                address: 0x3ff000005,
                ..target
            },
        )
        .is_err());
        assert!(focus_target(&windows, TargetIdentity { pid: 43, ..target }).is_err());
        let mut hidden = window(target.address, target.pid);
        hidden.hidden = true;
        assert!(focus_target(&[hidden], target).is_err());
    }

    #[test]
    fn active_focus_readback_preserves_full_address_and_owner() {
        let active = focus_from_active(&serde_json::json!({
            "address": "0x1234ff000005", "pid": 42,
        }))
        .unwrap()
        .unwrap();
        assert_eq!(
            active,
            TargetIdentity {
                address: 0x1234ff000005,
                pid: 42
            },
        );
        assert_eq!(focus_from_active(&serde_json::json!({})).unwrap(), None);
        for reply in [
            serde_json::json!({"address":"0xff000005"}),
            serde_json::json!({"address":"0x0","pid":42}),
            serde_json::json!({"address":"0x1234ff000005","pid":0}),
            serde_json::json!({"address":"0x1234ff000005","pid":-1}),
            serde_json::json!({"address":"0x1234ff000005","pid":u64::MAX}),
        ] {
            assert!(focus_from_active(&reply).is_err());
        }
    }

    #[test]
    fn accessibility_correlation_rejects_duplicate_compositor_titles() {
        let a = window(0x10, 42);
        let b = window(0x20, 42);
        assert!(accessibility_target(&[a.clone()], a.address, a.pid).is_some());
        assert!(accessibility_target(&[a.clone(), b], a.address, a.pid).is_none());
    }

    #[test]
    fn truncated_handle_collision_refuses_even_across_pids() {
        assert!(capture_target(
            &[window(0x100000010, 42), window(0x200000010, 43)],
            0x100000010,
            Some(42)
        )
        .is_err());
    }

    #[test]
    fn off_workspace_target_is_capturable_but_hidden_target_refuses() {
        let mut target = window(0x10, 42);
        target.visible = false;
        assert!(capture_target(&[target.clone()], 0x10, Some(42)).is_ok());
        target.hidden = true;
        assert!(capture_target(&[target], 0x10, Some(42)).is_err());
    }

    #[test]
    fn stalled_ipc_reply_is_bounded() {
        let (mut client, _server) = UnixStream::pair().unwrap();
        let start = Instant::now();
        assert!(read_reply(&mut client, b"j/clients", Duration::from_millis(30)).is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    fn replying_ipc(bytes: Vec<u8>, delay: Duration) -> UnixStream {
        let (client, mut server) = UnixStream::pair().unwrap();
        std::thread::spawn(move || {
            server
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut request = [0; 128];
            assert!(server.read(&mut request).unwrap() > 0);
            std::thread::sleep(delay);
            // An oversized reply may be rejected before the writer finishes.
            let _ = server.write_all(&bytes);
        });
        client
    }

    #[test]
    fn timed_out_observation_uses_fresh_connection_and_discards_partial_bytes() {
        let mut attempts = 0;
        let mut stalled = Vec::new();
        let value: serde_json::Value = query_with(
            "j/clients",
            Duration::from_millis(30),
            Duration::from_secs(1),
            Duration::from_millis(1),
            |_| {
                attempts += 1;
                if attempts == 1 {
                    let (client, mut server) = UnixStream::pair()?;
                    server.write_all(b"{\"stale\":")?;
                    stalled.push(server);
                    Ok(client)
                } else {
                    Ok(replying_ipc(b"[]".to_vec(), Duration::ZERO))
                }
            },
        )
        .unwrap();
        assert_eq!(value, serde_json::json!([]));
        assert_eq!(attempts, 2);
    }

    #[test]
    fn healthy_slow_observation_is_not_retried() {
        let mut attempts = 0;
        let value: serde_json::Value = query_with(
            "j/monitors",
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_millis(1),
            |_| {
                attempts += 1;
                Ok(replying_ipc(b"[]".to_vec(), Duration::from_millis(30)))
            },
        )
        .unwrap();
        assert_eq!(value, serde_json::json!([]));
        assert_eq!(attempts, 1);
    }

    #[test]
    fn observation_connection_and_attestation_errors_are_not_retried() {
        for timeout in [false, true] {
            let mut attempts = 0;
            let error = query_with::<serde_json::Value>(
                "j/clients",
                QUERY_TIMEOUT,
                QUERY_TOTAL_TIMEOUT,
                QUERY_RETRY_BACKOFF,
                |_| {
                    attempts += 1;
                    if timeout {
                        return Err(io::Error::from(io::ErrorKind::TimedOut).into());
                    }
                    bail!("could not verify same-user compositor peer");
                },
            )
            .unwrap_err();
            assert_eq!(attempts, 1);
            assert!(timeout || error.to_string().contains("same-user compositor peer"));
        }
    }

    #[test]
    fn observation_retry_must_pass_fresh_attestation() {
        let mut attempts = 0;
        let mut stalled = Vec::new();
        let error = query_with::<serde_json::Value>(
            "j/activewindow",
            Duration::from_millis(30),
            Duration::from_secs(1),
            Duration::from_millis(1),
            |_| {
                attempts += 1;
                if attempts == 2 {
                    bail!("Hyprland compositor changed during observation retry");
                }
                let (client, server) = UnixStream::pair()?;
                stalled.push(server);
                Ok(client)
            },
        )
        .unwrap_err();
        assert_eq!(attempts, 2);
        assert!(error.to_string().contains("compositor changed"));
    }

    #[test]
    fn malformed_truncated_and_oversized_observations_are_not_retried() {
        for bytes in [
            b"invalid".to_vec(),
            b"{\"pid\"".to_vec(),
            vec![b' '; MAX_REPLY_BYTES + 1],
        ] {
            let mut attempts = 0;
            assert!(query_with::<serde_json::Value>(
                "j/clients",
                QUERY_TIMEOUT,
                QUERY_TOTAL_TIMEOUT,
                QUERY_RETRY_BACKOFF,
                |_| {
                    attempts += 1;
                    Ok(replying_ipc(bytes.clone(), Duration::ZERO))
                },
            )
            .is_err());
            assert_eq!(attempts, 1);
        }
    }

    #[test]
    fn observation_total_deadline_includes_connect_and_read() {
        let mut attempts = 0;
        let mut stalled = Vec::new();
        let start = Instant::now();
        let error = query_with::<serde_json::Value>(
            "j/clients",
            Duration::from_millis(80),
            Duration::from_millis(110),
            Duration::from_millis(1),
            |deadline| {
                attempts += 1;
                assert!(
                    deadline.saturating_duration_since(Instant::now()) <= Duration::from_millis(80)
                );
                std::thread::sleep(Duration::from_millis(10));
                let (client, server) = UnixStream::pair()?;
                stalled.push(server);
                Ok(client)
            },
        )
        .unwrap_err();
        assert!(is_query_timeout(&error));
        assert!(attempts <= QUERY_MAX_ATTEMPTS);
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn observation_attempt_cap_applies_with_a_generous_total_budget() {
        let mut attempts = 0;
        let mut stalled = Vec::new();
        let error = query_with::<serde_json::Value>(
            "j/clients",
            Duration::from_millis(30),
            Duration::from_secs(10),
            Duration::from_millis(1),
            |_| {
                attempts += 1;
                let (client, server) = UnixStream::pair()?;
                stalled.push(server);
                Ok(client)
            },
        )
        .unwrap_err();
        assert!(is_query_timeout(&error));
        assert_eq!(attempts, 2);
    }

    #[test]
    fn observation_never_writes_after_connect_consumes_total_deadline() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let mut client = Some(client);
        let mut attempts = 0;
        let error = query_with::<serde_json::Value>(
            "j/clients",
            Duration::from_millis(10),
            Duration::from_millis(10),
            Duration::from_millis(1),
            |_| {
                attempts += 1;
                std::thread::sleep(Duration::from_millis(30));
                Ok(client.take().unwrap())
            },
        )
        .unwrap_err();
        assert!(is_query_timeout(&error));
        assert_eq!(attempts, 1);
        // The client was dropped without sending a request: EOF, not bytes.
        assert_eq!(server.read(&mut [0; 64]).unwrap(), 0);
    }

    #[test]
    fn input_and_unknown_json_commands_are_never_retried() {
        for command in ["dispatch nop", "j/dispatch nop", "j/unknown"] {
            let mut attempts = 0;
            let mut stalled = Vec::new();
            assert!(query_with::<serde_json::Value>(
                command,
                Duration::from_millis(30),
                Duration::from_secs(1),
                Duration::from_millis(1),
                |_| {
                    attempts += 1;
                    let (client, server) = UnixStream::pair()?;
                    stalled.push(server);
                    Ok(client)
                },
            )
            .is_err());
            assert_eq!(attempts, 1);
        }
    }

    #[test]
    fn observation_timeout_classifier_is_closed() {
        for (kind, expected) in [
            (io::ErrorKind::WouldBlock, true),
            (io::ErrorKind::TimedOut, true),
            (io::ErrorKind::ConnectionRefused, false),
            (io::ErrorKind::ConnectionReset, false),
            (io::ErrorKind::Interrupted, false),
            (io::ErrorKind::UnexpectedEof, false),
            (io::ErrorKind::PermissionDenied, false),
            (io::ErrorKind::InvalidData, false),
        ] {
            let error = anyhow::Error::from(io::Error::from(kind)).context("query");
            assert_eq!(is_query_timeout(&error), expected);
        }
        assert!(!is_query_timeout(&anyhow::anyhow!("identity unproven")));
        assert!(is_query_timeout(
            &query_time_remaining(Instant::now()).unwrap_err()
        ));
    }

    #[test]
    fn logical_resize_is_bounded_before_allocation() {
        assert!(valid_dimensions(3840, 2160));
        assert!(!valid_dimensions(0, 1));
        assert!(!valid_dimensions(u32::MAX, u32::MAX));
    }

    fn display_monitor() -> DisplayMonitor {
        serde_json::from_value(serde_json::json!({
            "width": 1920, "height": 1080, "scale": 1.0,
            "x": 0, "y": 0, "transform": 0,
        }))
        .unwrap()
    }

    #[test]
    fn display_identity_accepts_qualified_native_geometry() {
        assert_eq!(
            screen_size_from_monitors(vec![display_monitor()]).unwrap(),
            (1920, 1080, 1.0)
        );
    }

    #[test]
    fn display_identity_rejects_ambiguous_outputs_and_unsupported_frames() {
        assert!(screen_size_from_monitors(vec![]).is_err());
        assert!(screen_size_from_monitors(vec![display_monitor(), display_monitor()]).is_err());
        for scale in [0.0, 1.25, 2.0, f64::NAN, f64::INFINITY] {
            let mut monitor = display_monitor();
            monitor.scale = scale;
            assert!(screen_size_from_monitors(vec![monitor]).is_err());
        }
        for (x, y, transform) in [(100, 0, 0), (0, -100, 0), (0, 0, 1), (0, 0, 7)] {
            let mut monitor = display_monitor();
            (monitor.x, monitor.y, monitor.transform) = (x, y, transform);
            assert!(screen_size_from_monitors(vec![monitor]).is_err());
        }
    }

    #[test]
    fn display_identity_rejects_empty_oversized_and_missing_geometry() {
        for (width, height) in [(0, 1080), (1920, 0), (u32::MAX, u32::MAX)] {
            let mut monitor = display_monitor();
            (monitor.width, monitor.height) = (width, height);
            assert!(screen_size_from_monitors(vec![monitor]).is_err());
        }
        let value = serde_json::json!({
            "width": 1920, "height": 1080, "scale": 1.0, "x": 0, "y": 0,
        });
        assert!(serde_json::from_value::<DisplayMonitor>(value).is_err());
    }
}
