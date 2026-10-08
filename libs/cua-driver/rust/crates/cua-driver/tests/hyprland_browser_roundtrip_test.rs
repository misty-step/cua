//! One generic native-input/perception browser story; no browser tools or CDP.
//!
//! Run inside `omp-gui -- sh` after starting a disposable stock Hyprland and
//! private D-Bus/AT-SPI session. Enable accessibility (NO_AT_BRIDGE must not be 1).
//! Requires Chromium, hyprctl, wtype, native toplevel-export capture, and the
//! exact candidate Driver. Set CUA_TEST_REQUIRE_FIXTURES=1,
//! CUA_E2E_UNRESTRICTED_GUI=1, CUA_DRIVER_RS_ENABLE_WAYLAND=1,
//! XDG_SESSION_TYPE=wayland, CUA_TEST_DRIVER_BIN=/absolute/candidate/cua-driver,
//! and CUA_E2E_BROWSER_BIN=/absolute/path/to/chromium. Optionally set
//! CUA_E2E_ARTIFACT_DIR to retain the decoded images and independent page state.
//!
//! cargo test -p cua-driver --test hyprland_browser_roundtrip_test \
//!   generic_native_open_type_capture_observes_destination -- \
//!   --ignored --exact --nocapture --test-threads=1
//!
//! Foreground activation/readback is not compositor-atomic. Inputs are sent
//! once, never replayed; the real page's HTTP journal, native tree, and decoded
//! destination pixels establish the outcome, not an event-delivery ack.
//! Ctrl+N can report foreground_effect_unknown after creating a differently
//! focused window; only that post-dispatch uncertainty is allowed, and only
//! with independent discovery/readback of the newly opened exact window.
#![cfg(target_os = "linux")]

use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use cua_driver_testkit::observer::{NativeObserver, ObserverBackend, TargetWindow, TargetZ};
use cua_driver_testkit::{spawn_in_job, BrowserFixtureServer, Driver, McpDriver, ToolResponse};
use serde::Deserialize;
use serde_json::{json, Value};

const DESTINATION_BLUE: [u8; 3] = [19, 103, 149];
const DESTINATION_ORANGE: [u8; 3] = [241, 139, 37];

fn private_hyprland_preflight() -> (PathBuf, PathBuf) {
    // Refuse before any daemon/compositor connection on the workstation.
    assert_eq!(
        std::env::var("OMP_ENGINEER_DISPLAY").as_deref(),
        Ok("isolated-v1")
    );
    let namespace = std::env::var_os("OMP_ENGINEER_DISPLAY_NS")
        .expect("omp-gui must attest the private engineer mount namespace");
    assert_eq!(
        std::fs::read_link("/proc/self/ns/mnt").unwrap(),
        PathBuf::from(namespace)
    );
    for kind in ["mnt", "pid"] {
        assert_eq!(
            std::fs::read_link(format!("/proc/self/ns/{kind}")).unwrap(),
            std::fs::read_link(format!("/proc/1/ns/{kind}")).unwrap()
        );
    }
    let uid = std::fs::metadata("/proc/self").unwrap().uid();
    assert_ne!(uid, 0, "the fixture must be unprivileged");
    assert_eq!(std::fs::metadata("/proc/1").unwrap().uid(), uid);
    for name in [
        "CUA_TEST_REQUIRE_FIXTURES",
        "CUA_E2E_UNRESTRICTED_GUI",
        "CUA_DRIVER_RS_ENABLE_WAYLAND",
    ] {
        assert_eq!(
            std::env::var(name).as_deref(),
            Ok("1"),
            "{name} is required"
        );
    }
    assert_eq!(std::env::var("XDG_SESSION_TYPE").as_deref(), Ok("wayland"));
    assert!(std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some_and(|s| !s.is_empty()));
    let display =
        PathBuf::from(std::env::var_os("WAYLAND_DISPLAY").expect("named Wayland display"));
    assert!(
        !display.is_absolute() && display.components().count() == 1,
        "the Wayland socket must be named within the private runtime directory"
    );
    let runtime = std::fs::canonicalize(
        std::env::var_os("XDG_RUNTIME_DIR").expect("private XDG_RUNTIME_DIR"),
    )
    .expect("private runtime directory must exist");
    assert!(runtime.starts_with("/tmp") || runtime.starts_with("/run"));
    assert!(runtime.is_dir());
    for name in ["WAYLAND_SOCKET", "SWAYSOCK", "CUA_INJECT_SOCKET"] {
        assert!(
            std::env::var_os(name).is_none(),
            "{name} must not select another input route"
        );
    }
    assert_ne!(std::env::var("NO_AT_BRIDGE").as_deref(), Ok("1"));
    assert_ne!(
        std::env::var("CUA_DRIVER_EXPERIMENTAL_HYPRLAND_INPUT").as_deref(),
        Ok("1")
    );
    assert!(
        !platform_linux::wayland::hyprland_input::enabled(),
        "this stock-Hyprland story must not have an input plugin socket"
    );
    let driver = PathBuf::from(
        std::env::var_os("CUA_TEST_DRIVER_BIN")
            .expect("explicit exact-candidate Driver is required"),
    );
    let browser = PathBuf::from(
        std::env::var_os("CUA_E2E_BROWSER_BIN").expect("explicit Chromium executable is required"),
    );
    for binary in [&driver, &browser] {
        assert!(
            binary.is_absolute() && binary.is_file(),
            "missing executable: {binary:?}"
        );
    }
    (driver, browser)
}

#[derive(Debug, Deserialize)]
struct CompositorWindow {
    address: String,
    pid: u32,
    mapped: bool,
    hidden: bool,
    xwayland: bool,
}

impl CompositorWindow {
    fn id(&self) -> u64 {
        let id = u64::from_str_radix(
            self.address.strip_prefix("0x").expect("full hex address"),
            16,
        )
        .expect("Hyprland address must fit u64");
        assert_ne!(id, 0);
        id
    }

    fn target(&self) -> TargetWindow {
        assert!(!self.xwayland, "Chromium must be native Wayland: {self:?}");
        assert!(
            self.id() > u64::from(u32::MAX),
            "a truncated/foreign ID is not a compositor address"
        );
        TargetWindow {
            pid: self.pid,
            native_id: self.id(),
        }
    }
}

fn compositor_windows() -> Vec<CompositorWindow> {
    let output = Command::new("hyprctl")
        .args(["-j", "clients"])
        .stdin(Stdio::null())
        .output()
        .expect("independent hyprctl enumeration");
    assert!(output.status.success(), "hyprctl clients: {output:?}");
    serde_json::from_slice(&output.stdout).expect("Hyprland clients JSON")
}

fn discover_fresh_window(pid: u32, before: &HashSet<u64>) -> TargetWindow {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let windows = compositor_windows();
        let candidates: Vec<_> = windows
            .iter()
            .filter(|w| w.pid == pid && w.mapped && !w.hidden && !before.contains(&w.id()))
            .collect();
        assert!(
            candidates.len() <= 1,
            "ambiguous fresh same-PID windows: {candidates:?}"
        );
        if let [window] = candidates.as_slice() {
            return window.target();
        }
        assert!(
            Instant::now() < deadline,
            "no fresh native Chromium window for pid {pid}: {windows:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn confirm_driver_binding(driver: &mut McpDriver, target: TargetWindow) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let listed = driver.call("list_windows", json!({"pid": target.pid}));
        assert!(!listed.is_error(), "list_windows: {}", listed.text());
        let matches = listed.structured()["windows"]
            .as_array()
            .expect("native window inventory")
            .iter()
            .filter(|w| {
                w["pid"].as_u64() == Some(u64::from(target.pid))
                    && w["window_id"].as_u64() == Some(target.native_id)
            })
            .count();
        assert!(
            matches <= 1,
            "duplicate exact window identity: {}",
            listed.structured()
        );
        if matches == 1 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "Driver never exposed independently discovered target {target:?}: {}",
            listed.structured()
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn assert_foreground(observer: &NativeObserver, target: TargetWindow) {
    let state = observer
        .snapshot(target)
        .expect("independent Hyprland focus readback");
    assert_eq!(state.foreground, Some(target.native_id));
    assert_eq!(state.input_focus, Some(target.native_id));
    assert_eq!(state.target_z, TargetZ::Foreground);
}

fn foreground_action(driver: &mut McpDriver, target: TargetWindow, tool: &str, fields: Value) {
    let mut args = json!({
        "pid": target.pid, "window_id": target.native_id, "delivery_mode": "foreground"
    });
    let Value::Object(fields) = fields else {
        panic!("foreground action fields must be an object");
    };
    args.as_object_mut().unwrap().extend(fields);
    let result = driver.call(tool, args);
    assert!(!result.is_error(), "{tool} refused: {}", result.text());
    assert_eq!(
        result.action_route(),
        Some("global_input"),
        "{}",
        result.structured()
    );
    assert_eq!(
        result.action_delivery_mode(),
        Some("foreground"),
        "{}",
        result.structured()
    );
}

fn fixture_html(marker: &str, loaded_marker: &str) -> String {
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8">
<title>Native generic browser destination</title>
<style>
html, body {{ margin: 0; min-height: 100vh; background: rgb(19,103,149); color: white; }}
main {{ padding: 24px; position: relative; z-index: 1; }}
h1 {{ font: bold 24px sans-serif; overflow-wrap: anywhere; }}
p {{ font: 18px sans-serif; overflow-wrap: anywhere; }}
#visual-target {{ position: fixed; right: 0; bottom: 0; width: 35vw; height: 45vh;
                    background: rgb(241,139,37); }}
</style></head><body><main><h1 id="destination">{marker}</h1>
<p id="loaded">Waiting for browser load</p></main><div id="visual-target"></div>
<script>
window.addEventListener('load', () => {{
    document.getElementById('loaded').textContent = {loaded_json};
    requestAnimationFrame(() => requestAnimationFrame(() => {{
        const bounds = document.getElementById('destination').getBoundingClientRect();
        const page = {{
            text: document.getElementById('destination').textContent,
            loadedMarker: document.getElementById('loaded').textContent,
            readyState: document.readyState, url: location.href,
            visibility: document.visibilityState,
            markerWidth: bounds.width, markerHeight: bounds.height,
            navigationType: performance.getEntriesByType('navigation')[0]?.type
        }};
        fetch(window.__CUA_E2E_FIXTURE_JOURNAL_URL, {{
            method: 'POST', body: JSON.stringify({{ page }})
        }});
    }}));
}});
</script></body></html>"#,
        loaded_json = serde_json::to_string(loaded_marker).unwrap()
    )
}

fn wait_for_real_page(
    server: &BrowserFixtureServer,
    url: &str,
    marker: &str,
    loaded: &str,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let state = server.snapshot();
        let page = &state["page"];
        if page["text"] == marker && page["loadedMarker"] == loaded && page["url"] == url {
            assert_eq!(page["readyState"], "complete");
            assert_eq!(page["visibility"], "visible");
            assert_eq!(page["navigationType"], "navigate");
            assert!(page["markerWidth"].as_f64().unwrap_or(0.0) > 0.0);
            assert!(page["markerHeight"].as_f64().unwrap_or(0.0) > 0.0);
            return state;
        }
        assert!(
            Instant::now() < deadline,
            "typed navigation never reached the real fixture: {state}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn window_state(
    driver: &mut McpDriver,
    target: TargetWindow,
    evidence: &Path,
    name: &str,
) -> (ToolResponse, image::RgbaImage) {
    let state = driver.call(
        "get_window_state",
        json!({"pid": target.pid, "window_id": target.native_id}),
    );
    std::fs::write(
        evidence.join(format!("{name}.json")),
        serde_json::to_vec_pretty(state.structured()).unwrap(),
    )
    .unwrap();
    assert!(!state.is_error(), "get_window_state: {}", state.text());
    assert_eq!(
        state.structured()["window_id"].as_u64(),
        Some(target.native_id)
    );
    assert_eq!(
        state.structured()["pid"].as_u64(),
        Some(u64::from(target.pid))
    );
    assert!(
        state.structured().get("screenshot_error").is_none(),
        "{}",
        state.structured()
    );
    let content = state.raw["result"]["content"]
        .as_array()
        .expect("MCP perception content");
    let data = content
        .iter()
        .find(|item| item["type"] == "image" && item["mimeType"] == "image/png")
        .and_then(|item| item["data"].as_str())
        .expect("the generic MCP response must contain a PNG, not a frame-only ack");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .expect("PNG base64");
    std::fs::write(evidence.join(format!("{name}.png")), &bytes).unwrap();
    let image = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
        .expect("decode real native capture PNG")
        .to_rgba8();
    assert!(image.width() > 0 && image.height() > 0, "zero-size PNG");
    assert_eq!(
        state.structured()["screenshot_width"].as_u64(),
        Some(u64::from(image.width()))
    );
    assert_eq!(
        state.structured()["screenshot_height"].as_u64(),
        Some(u64::from(image.height()))
    );
    (state, image)
}

fn color_fraction(image: &image::RgbaImage, color: [u8; 3]) -> f64 {
    let matches = image
        .pixels()
        .filter(|pixel| {
            pixel[3] >= 240
                && color
                    .iter()
                    .enumerate()
                    .all(|(channel, value)| pixel[channel].abs_diff(*value) <= 8)
        })
        .count();
    matches as f64 / (u64::from(image.width()) * u64::from(image.height())) as f64
}

#[test]
#[ignore = "requires disposable stock Hyprland, private AT-SPI, Chromium, and exact-candidate Driver"]
fn generic_native_open_type_capture_observes_destination() {
    let (driver_bin, browser_bin) = private_hyprland_preflight();
    let artifact_root = std::env::var_os("CUA_E2E_ARTIFACT_DIR");
    let evidence = if let Some(root) = &artifact_root {
        std::fs::create_dir_all(root).expect("create requested evidence root");
        tempfile::Builder::new()
            .prefix("hyprland-browser-roundtrip-")
            .tempdir_in(root)
    } else {
        tempfile::tempdir()
    }
    .expect("isolated capture evidence");
    let evidence_path = evidence.path().to_path_buf();
    let _cleanup = if artifact_root.is_some() {
        let _ = evidence.keep();
        None
    } else {
        Some(evidence)
    };
    // Declare the profile before Driver: its owned browser tree is reaped before
    // profile deletion on both success and assertion failure.
    let profile = tempfile::Builder::new()
        .prefix("cua-native-browser-")
        .tempdir()
        .unwrap();
    let run = profile.path().file_name().unwrap().to_str().unwrap();
    let marker = format!("KAYLEE_NATIVE_DESTINATION_{run}");
    let loaded = format!("page_load=complete:{run}");
    let server = BrowserFixtureServer::start(&fixture_html(&marker, &loaded));
    let url = format!("{}?run={run}", server.page_url());
    assert_eq!(
        server.snapshot(),
        json!({}),
        "readiness probes cannot count as browser loads"
    );
    let mut driver = McpDriver::spawn_named("hyprland-generic-browser-roundtrip")
        .expect("commissioned story requires the exact-candidate Driver; no skip-as-pass");
    let observer = NativeObserver::new();
    let before: HashSet<_> = compositor_windows()
        .iter()
        .map(CompositorWindow::id)
        .collect();
    let mut command = Command::new(&browser_bin);
    command
        .arg(format!("--user-data-dir={}", profile.path().display()))
        .args([
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-background-networking",
            "--disable-component-update",
            "--disable-extensions",
            "--disable-default-apps",
            "--password-store=basic",
            "--ozone-platform=wayland",
            "--force-renderer-accessibility",
            "--new-window",
            "--window-size=980,760",
        ])
        .env_remove("DISPLAY")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    if std::env::var("CUA_E2E_BROWSER_NO_SANDBOX").as_deref() == Ok("1") {
        command.arg("--no-sandbox");
    }
    command.arg("about:blank");
    let child = spawn_in_job(&mut command).expect("launch fresh native Chromium");
    let pid = child.id();
    driver.reaper().push(child);
    let initial = discover_fresh_window(pid, &before);
    confirm_driver_binding(&mut driver, initial);
    assert_foreground(&observer, initial);

    let before_new: HashSet<_> = compositor_windows()
        .iter()
        .map(CompositorWindow::id)
        .collect();
    let opened = driver.call(
        "hotkey",
        json!({
            "pid": initial.pid, "window_id": initial.native_id,
            "keys": ["ctrl", "n"], "delivery_mode": "foreground"
        }),
    );
    std::fs::write(
        evidence_path.join("open-window-result.json"),
        serde_json::to_vec_pretty(&opened.raw).unwrap(),
    )
    .unwrap();
    if opened.is_error() {
        assert!(
            opened.text().starts_with("foreground_effect_unknown:"),
            "Ctrl+N failed rather than reporting post-dispatch focus uncertainty: {}",
            opened.text()
        );
    } else {
        assert_eq!(
            opened.action_route(),
            Some("global_input"),
            "{}",
            opened.structured()
        );
        assert_eq!(
            opened.action_delivery_mode(),
            Some("foreground"),
            "{}",
            opened.structured()
        );
    }
    // Acknowledgement alone cannot establish opening; do not replay an
    // uncertain chord. The new compositor identity must actually appear.
    let destination = discover_fresh_window(pid, &before_new);
    assert_eq!(
        destination.pid, initial.pid,
        "retain the exact same-PID sibling contract"
    );
    assert_ne!(destination.native_id, initial.native_id);
    confirm_driver_binding(&mut driver, destination);
    assert_foreground(&observer, destination);
    std::fs::write(
        evidence_path.join("window-identities.json"),
        serde_json::to_vec_pretty(&json!({
            "driver": driver_bin, "browser": browser_bin, "pid": pid,
            "initial_address": format!("0x{:x}", initial.native_id),
            "destination_address": format!("0x{:x}", destination.native_id),
            "typed_url": url, "destination_marker": marker
        }))
        .unwrap(),
    )
    .unwrap();

    foreground_action(
        &mut driver,
        destination,
        "hotkey",
        json!({"keys": ["ctrl", "l"]}),
    );
    foreground_action(&mut driver, destination, "type_text", json!({"text": url}));
    foreground_action(
        &mut driver,
        destination,
        "press_key",
        json!({"key": "Return"}),
    );
    let page_state = wait_for_real_page(&server, &url, &marker, &loaded);
    std::fs::write(
        evidence_path.join("page-state.json"),
        serde_json::to_vec_pretty(&page_state).unwrap(),
    )
    .unwrap();

    // Only perception is polled while Chromium's AT-SPI tree/paint settles;
    // uncertain foreground input is never retried.
    let deadline = Instant::now() + Duration::from_secs(10);
    let (blue, orange) = loop {
        let (state, image) = window_state(&mut driver, destination, &evidence_path, "destination");
        let blue = color_fraction(&image, DESTINATION_BLUE);
        let orange = color_fraction(&image, DESTINATION_ORANGE);
        if state.tree_text().contains(&marker)
            && state.tree_text().contains(&loaded)
            && blue >= 0.10
            && orange >= 0.01
        {
            break (blue, orange);
        }
        assert!(Instant::now() < deadline, "destination content absent from exact native tree/image: blue={blue}, orange={orange}, tree={}", state.tree_text());
        thread::sleep(Duration::from_millis(100));
    };
    // A first-sibling capture or first-PID input shortcut must not pass by
    // navigating/capturing the original browser instead of the Ctrl+N window.
    let (sibling_state, sibling_image) =
        window_state(&mut driver, initial, &evidence_path, "initial-sibling");
    assert!(
        !sibling_state.tree_text().contains(&marker),
        "destination leaked into initial sibling tree"
    );
    assert!(
        color_fraction(&sibling_image, DESTINATION_BLUE) < 0.01,
        "initial sibling returned destination pixels"
    );
    assert_foreground(&observer, destination);
    eprintln!("native browser roundtrip: target={:#x} pid={pid}, blue={blue:.3}, orange={orange:.3}, evidence={}", destination.native_id, evidence_path.display());
}
