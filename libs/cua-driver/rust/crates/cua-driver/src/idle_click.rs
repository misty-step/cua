//! Trusted-launcher, single-use X11 input. No general platform runtime is created.

use std::path::PathBuf;

pub const POLICY_ENV: &str = "CUA_DRIVER_IDLE_CLICK_POLICY";
pub const CAPABILITY: &str = "input.pointer.click.x11_send_event_idle_v1";

pub fn configured_path() -> Option<String> {
    std::env::var_os(POLICY_ENV).map(|s| s.into_string().unwrap_or_default())
}

pub fn entrypoint() -> Option<anyhow::Result<()>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = args.iter().position(|s| s == "--idle-click-policy");
    let env = configured_path();
    if flag.is_none() && env.is_none() {
        return None;
    }
    if args.first().is_some_and(|s| {
        matches!(
            s.as_str(),
            "manifest" | "--help" | "-h" | "--version" | "-V"
        )
    }) && flag.is_none()
    {
        return None;
    }
    Some((|| {
        let path = match flag {
            Some(i) => {
                let value = args
                    .get(i + 1)
                    .filter(|s| !s.starts_with('-'))
                    .ok_or_else(|| anyhow::anyhow!("--idle-click-policy requires a path"))?;
                anyhow::ensure!(
                    env.as_ref().is_none_or(|s| s == value),
                    "conflicting idle-click policy paths"
                );
                value.clone()
            }
            None => env.expect("checked above"),
        };
        anyhow::ensure!(
            args.first().is_some_and(|s| s == "mcp"),
            "idle-click policy is supported only by direct mcp"
        );
        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--idle-click-policy" if Some(i) == flag => i += 2,
                "--no-overlay" | "--direct" => i += 1,
                _ => anyhow::bail!("unsupported option in idle-click mode: {}", args[i]),
            }
        }
        anyhow::ensure!(
            !args.iter().any(|s| matches!(
                s.as_str(),
                "--socket"
                    | "--grant"
                    | "--experimental-history"
                    | "--claude-code-computer-use-compat"
            )),
            "idle-click mode cannot proxy, grant or widen its surface"
        );
        #[cfg(target_os = "linux")]
        {
            crate::configure_startup_permission_mode(None, false, None, false, &[])?;
            cua_driver_core::authorization::validate_startup_authorization()?;
            cua_driver_core::policy::validate_configured_policy()?;
            let guard = linux::Guard::load(PathBuf::from(path))?;
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(crate::idle_click_mcp::run(guard))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = path;
            anyhow::bail!("idle-click SendEvent mode requires Linux X11/XWayland")
        }
    })())
}

#[cfg(target_os = "linux")]
pub mod linux {
    use anyhow::{ensure, Context, Result};
    use serde::Deserialize;
    use serde_json::{json, Value};
    use std::{
        fs::OpenOptions,
        io::Read,
        os::unix::fs::{MetadataExt, OpenOptionsExt},
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex,
        },
        thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };
    use x11rb::{
        connection::Connection,
        protocol::{
            res::{self, ClientIdMask, ClientIdSpec},
            xproto::*,
            Event,
        },
        rust_connection::RustConnection,
    };

    #[derive(Clone, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct Policy {
        pub schema: u32,
        pub verdict_path: PathBuf,
        pub display: String,
        pub producer_pid: u32,
        pub pid: u32,
        pub window_id: u32,
        pub max_age_ms: u32,
    }

    #[derive(Clone, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct Click {
        pub pid: u32,
        pub window_id: u32,
        pub x: i16,
        pub y: i16,
        #[serde(default = "left")]
        pub button: String,
        #[serde(default = "one")]
        pub count: u32,
        #[serde(default)]
        #[serde(rename = "session")]
        pub _session: Option<String>,
        #[serde(default)]
        pub delivery_mode: Option<String>,
    }
    fn left() -> String {
        "left".into()
    }
    fn one() -> u32 {
        1
    }

    pub struct Guard {
        pub policy: Policy,
        conn: RustConnection,
        producer_start: String,
        target_start: String,
        destroyed: AtomicBool,
        latest_verdict: Mutex<f64>,
    }

    #[derive(Default)]
    pub struct Revocation {
        revoked: AtomicBool,
        send: Mutex<()>,
    }
    impl Revocation {
        pub fn revoke(&self) {
            let _send = self.send.lock().unwrap_or_else(|e| e.into_inner());
            self.revoked.store(true, Ordering::Release);
        }
        pub fn is_revoked(&self) -> bool {
            self.revoked.load(Ordering::Acquire)
        }
    }

    struct OwnedPress<'a> {
        guard: &'a Guard,
        event: ButtonPressEvent,
        retired: bool,
    }

    // X11 IDs can be reused. Check ownership and send in one short server
    // transaction, never holding the server across the paired-click interval.
    struct ServerTransaction<'a>(&'a RustConnection);
    impl<'a> ServerTransaction<'a> {
        fn begin(conn: &'a RustConnection) -> Result<Self> {
            conn.grab_server()?.check()?;
            Ok(Self(conn))
        }
    }
    impl Drop for ServerTransaction<'_> {
        fn drop(&mut self) {
            if let Ok(cookie) = self.0.ungrab_server() {
                let _ = cookie.check();
            }
        }
    }

    impl OwnedPress<'_> {
        fn release(&mut self) -> Result<()> {
            if self.retired {
                return Ok(());
            }
            self.retired = true; // Never replay a release whose outcome is uncertain.
            let _transaction = ServerTransaction::begin(&self.guard.conn)?;
            self.guard.conn.get_input_focus()?.reply()?;
            while let Some(event) = self.guard.conn.poll_for_event()? {
                if matches!(event, Event::DestroyNotify(e) if e.window == self.event.event) {
                    self.guard.destroyed.store(true, Ordering::Release);
                }
            }
            ensure!(
                !self.guard.destroyed.load(Ordering::Acquire),
                "owned target destroyed; never release to a reused XID"
            );
            let release = ButtonReleaseEvent {
                response_type: BUTTON_RELEASE_EVENT,
                state: match self.event.detail {
                    1 => KeyButMask::BUTTON1,
                    2 => KeyButMask::BUTTON2,
                    _ => KeyButMask::BUTTON3,
                },
                ..self.event
            };
            self.guard
                .conn
                .send_event(false, self.event.event, EventMask::BUTTON_RELEASE, release)?
                .check()?;
            self.guard.conn.get_input_focus()?.reply()?;
            Ok(())
        }
    }

    impl Drop for OwnedPress<'_> {
        fn drop(&mut self) {
            let _ = self.release();
        }
    }

    fn private_json(path: &Path) -> Result<Value> {
        ensure!(path.is_absolute(), "policy paths must be absolute");
        let mut f = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        let m = f.metadata()?;
        ensure!(
            m.is_file()
                && m.uid() == unsafe { libc::geteuid() }
                && m.mode() & 0o077 == 0
                && m.len() <= 16384,
            "invalid private regular file"
        );
        let mut bytes = Vec::new();
        (&mut f).take(16385).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 16384, "oversized policy/verdict");
        Ok(serde_json::from_slice(&bytes)?)
    }

    fn process_start(pid: u32) -> Result<String> {
        ensure!(pid > 0, "pid must be positive");
        let path = format!("/proc/{pid}");
        ensure!(
            std::fs::metadata(&path)?.uid() == unsafe { libc::geteuid() },
            "target/producer must belong to the launcher uid"
        );
        let stat = std::fs::read_to_string(format!("{path}/stat"))?;
        let fields: Vec<_> = stat
            .rsplit_once(')')
            .context("invalid proc identity")?
            .1
            .split_whitespace()
            .collect();
        ensure!(
            fields.first().is_some_and(|s| *s != "Z" && *s != "X"),
            "process is not live"
        );
        Ok(fields.get(19).context("missing process start")?.to_string())
    }

    fn now() -> Result<f64> {
        Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64())
    }

    pub fn validate_verdict(v: &Value, p: &Policy, at: f64) -> Result<u64> {
        ensure!(
            v["schema"].as_u64() == Some(1)
                && v["producer_pid"].as_u64() == Some(p.producer_pid.into()),
            "unknown producer/schema"
        );
        let written = v["at"].as_f64().context("missing verdict time")?;
        let since = v["since"].as_f64().context("missing away episode")?;
        ensure!(
            written.is_finite()
                && since.is_finite()
                && since > 0.0
                && since <= written
                && written <= at
                && at - written <= f64::from(p.max_age_ms) / 1000.0,
            "stale/future verdict or episode"
        );
        ensure!(
            v["input_mode"] == "all-input"
                && v["present"] == false
                && v["drivable"] == true
                && v["reasons"] == json!(["idle"])
                && v["degraded"] == json!([]),
            "idle permission unavailable"
        );
        ensure!(
            v["evidence"]["locked"] == false
                && v["evidence"]["meeting"] == false
                && v["evidence"]["idle_seconds"]
                    .as_f64()
                    .is_some_and(|s| s.is_finite() && s >= 600.0),
            "unknown/locked/non-idle evidence"
        );
        Ok(since.to_bits())
    }

    impl Guard {
        pub fn load(path: PathBuf) -> Result<Arc<Self>> {
            let policy: Policy = serde_json::from_value(private_json(&path)?)?;
            ensure!(
                policy.schema == 1 && (1..=15000).contains(&policy.max_age_ms),
                "unsupported policy schema/freshness bound"
            );
            ensure!(
                policy.display.starts_with(':')
                    && policy.display[1..].bytes().all(|b| b.is_ascii_digit())
                    && policy.display.len() > 1,
                "local Unix X11 display required"
            );
            ensure!(
                std::env::var("DISPLAY").ok().as_deref() == Some(policy.display.as_str())
                    && std::env::var_os("XAUTHORITY").is_some(),
                "explicit matching DISPLAY/XAUTHORITY required; no host discovery"
            );
            ensure!(policy.window_id > 0, "exact window required");
            let producer_start = process_start(policy.producer_pid)?;
            let target_start = process_start(policy.pid)?;
            let (conn, _) = RustConnection::connect(Some(&policy.display))?;
            res::query_version(&conn, 1, 2)?.reply()?;
            let guard = Arc::new(Self {
                policy,
                conn,
                producer_start,
                target_start,
                destroyed: AtomicBool::new(false),
                latest_verdict: Mutex::new(0.0),
            });
            let transaction = ServerTransaction::begin(&guard.conn)?;
            guard.target()?;
            guard
                .conn
                .change_window_attributes(
                    guard.policy.window_id,
                    &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
                )?
                .check()?;
            drop(transaction);
            Ok(guard)
        }

        fn target(&self) -> Result<GetGeometryReply> {
            while let Some(event) = self.conn.poll_for_event()? {
                if matches!(event, Event::DestroyNotify(e) if e.window == self.policy.window_id) {
                    self.destroyed.store(true, Ordering::Release);
                }
            }
            ensure!(
                !self.destroyed.load(Ordering::Acquire),
                "target was destroyed; XID reuse is forbidden"
            );
            ensure!(
                process_start(self.policy.pid)? == self.target_start,
                "target process changed"
            );
            let ids = res::query_client_ids(
                &self.conn,
                &[ClientIdSpec {
                    client: self.policy.window_id,
                    mask: ClientIdMask::LOCAL_CLIENT_PID,
                }],
            )?
            .reply()?;
            ensure!(
                ids.ids
                    .iter()
                    .any(|id| id.spec.mask == ClientIdMask::LOCAL_CLIENT_PID
                        && id.value == [self.policy.pid]),
                "XRes owner mismatch/unsupported"
            );
            ensure!(
                self.conn
                    .get_window_attributes(self.policy.window_id)?
                    .reply()?
                    .map_state
                    == MapState::VIEWABLE,
                "target is unreachable"
            );
            Ok(self.conn.get_geometry(self.policy.window_id)?.reply()?)
        }

        fn episode(&self) -> Result<u64> {
            ensure!(
                process_start(self.policy.producer_pid)? == self.producer_start,
                "producer restarted/stopped"
            );
            let verdict = private_json(&self.policy.verdict_path)?;
            let episode = validate_verdict(&verdict, &self.policy, now()?)?;
            let at = verdict["at"].as_f64().context("missing verdict time")?;
            let mut latest = self
                .latest_verdict
                .lock()
                .map_err(|_| anyhow::anyhow!("verdict clock poisoned"))?;
            ensure!(at >= *latest, "verdict moved backwards");
            *latest = at;
            Ok(episode)
        }

        fn fence(&self, episode: u64, cancel: &Revocation) -> Result<()> {
            let result = (|| {
                ensure!(!cancel.is_revoked(), "operation revoked");
                ensure!(self.episode()? == episode, "away episode changed");
                self.target()?;
                ensure!(
                    !cancel.is_revoked() && self.episode()? == episode,
                    "permission changed after resolution"
                );
                Ok(())
            })();
            if result.is_err() {
                cancel.revoke();
            }
            result
        }

        pub fn metadata(&self) -> Result<Value> {
            self.episode()?;
            let g = self.target()?;
            self.episode()?;
            Ok(
                json!({"pid":self.policy.pid,"window_id":self.policy.window_id,"app_name":"Idle-gated owned target","title":"Idle-gated owned target","off_screen":false,"z_index":0,"width":g.width,"height":g.height}),
            )
        }

        pub fn click(&self, args: Value, cancel: Arc<Revocation>) -> Value {
            let mut pressed = false;
            let mut released = false;
            let mut failed = false;
            let outcome = (|| -> Result<()> {
                let input: Click = serde_json::from_value(args.clone())?;
                ensure!(
                    input.pid == self.policy.pid
                        && input.window_id == self.policy.window_id
                        && input.count == 1
                        && input
                            .delivery_mode
                            .as_deref()
                            .is_none_or(|s| s == "background"),
                    "unsupported input or wrong target"
                );
                let button = match input.button.as_str() {
                    "left" => 1,
                    "middle" => 2,
                    "right" => 3,
                    _ => anyhow::bail!("unsupported button"),
                };
                let episode = self.episode()?;
                let g = self.target()?;
                ensure!(
                    input.x >= 0
                        && input.y >= 0
                        && (input.x as u16) < g.width
                        && (input.y as u16) < g.height,
                    "coordinates outside exact target"
                );
                cua_driver_core::policy::authorize_tool_call("click", &args)?;
                self.fence(episode, &cancel)?;
                let position = self
                    .conn
                    .translate_coordinates(input.window_id, g.root, input.x, input.y)?
                    .reply()?;
                let event = ButtonPressEvent {
                    response_type: BUTTON_PRESS_EVENT,
                    detail: button,
                    sequence: 0,
                    time: x11rb::CURRENT_TIME,
                    root: g.root,
                    event: input.window_id,
                    child: x11rb::NONE,
                    root_x: position.dst_x,
                    root_y: position.dst_y,
                    event_x: input.x,
                    event_y: input.y,
                    state: KeyButMask::from(0u16),
                    same_screen: true,
                };
                // Final read follows all X11/authorization waits. No actuator fallback.
                self.fence(episode, &cancel)?;
                let transaction = ServerTransaction::begin(&self.conn)?;
                let current = self.target()?;
                ensure!(
                    (input.x as u16) < current.width && (input.y as u16) < current.height,
                    "target geometry changed"
                );
                let send = cancel
                    .send
                    .lock()
                    .map_err(|_| anyhow::anyhow!("send boundary poisoned"))?;
                ensure!(
                    !cancel.is_revoked() && self.episode()? == episode,
                    "revoked at press boundary"
                );
                let mut owned = OwnedPress {
                    guard: self,
                    event,
                    retired: false,
                };
                pressed = true; // possibly committed, even on a transport error
                let press_result = (|| -> Result<()> {
                    let cookie = self.conn.send_event(
                        false,
                        input.window_id,
                        EventMask::BUTTON_PRESS,
                        event,
                    )?;
                    self.conn.flush()?;
                    drop(send);
                    cookie.check()?;
                    Ok(())
                })();
                drop(transaction);
                if press_result.is_err() {
                    failed = true;
                    cancel.revoke();
                }
                let deadline = Instant::now() + Duration::from_millis(35);
                while !cancel.is_revoked() && Instant::now() < deadline {
                    if self.fence(episode, &cancel).is_err() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(1));
                }
                // Cleanup is exempt only for this operation's possibly committed press.
                released = owned.release().is_ok();
                if !released {
                    failed = true;
                    cancel.revoke();
                }
                if self.episode().ok() != Some(episode) {
                    cancel.revoke();
                }
                press_result?;
                Ok(())
            })();
            if outcome.is_err() {
                failed = true;
                cancel.revoke();
            }
            let cancelled = cancel.is_revoked();
            let code = if pressed && !released {
                "idle_click_cleanup_uncertain"
            } else if pressed && cancelled {
                "idle_click_cancelled"
            } else if failed || cancelled {
                "idle_click_refused"
            } else {
                "idle_click_complete"
            };
            json!({"content":[{"type":"text","text":code}],"isError":cancelled || failed,"structuredContent":{"code":code,"effect":if pressed && cancelled {"partial"} else if cancelled {"refused"} else {"unverifiable"},"route":"synthetic_events","path":"x11_send_event","delivery_mode":"background","guard_capability":super::CAPABILITY,"press_sent":pressed,"release_sent":released,"cleanup_complete":!pressed || released,"retired":true}})
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::linux::{validate_verdict, Click, Policy, Revocation};
    use serde_json::json;

    #[test]
    fn idle_click_verdict_requires_all_input_unlocked_fresh_episode() {
        let policy = Policy {
            schema: 1,
            verdict_path: "/private/verdict".into(),
            display: ":71".into(),
            producer_pid: 10,
            pid: 20,
            window_id: 30,
            max_age_ms: 15000,
        };
        let valid = json!({"schema":1,"producer_pid":10,"input_mode":"all-input","at":1000.0,
            "since":900.0,"present":false,"drivable":true,"reasons":["idle"],"degraded":[],
            "evidence":{"locked":false,"meeting":false,"idle_seconds":600.0}});
        assert!(validate_verdict(&valid, &policy, 1001.0).is_ok());
        let patches = [
            json!({"present":true}),
            json!({"drivable":false}),
            json!({"at":980.0}),
            json!({"at":1002.0}),
            json!({"since":1002.0}),
            json!({"since":null}),
            json!({"schema":true}),
            json!({"producer_pid":11}),
            json!({"input_mode":"temporal-discount"}),
            json!({"degraded":["input"]}),
            json!({"reasons":["locked"]}),
            json!({"evidence":{"locked":true,"meeting":false,"idle_seconds":700}}),
            json!({"evidence":{"locked":null,"meeting":false,"idle_seconds":700}}),
            json!({"evidence":{"locked":false,"meeting":true,"idle_seconds":700}}),
            json!({"evidence":{"locked":false,"meeting":false,"idle_seconds":599}}),
        ];
        for patch in patches {
            let mut value = valid.clone();
            for (key, value_patch) in patch.as_object().unwrap() {
                value[key] = value_patch.clone();
            }
            assert!(
                validate_verdict(&value, &policy, 1001.0).is_err(),
                "{patch}"
            );
        }
    }

    #[test]
    fn idle_click_model_cannot_supply_guard_or_broader_input_properties() {
        let valid = json!({"pid":20,"window_id":30,"x":0,"y":1,"session":"generated"});
        assert!(serde_json::from_value::<Click>(valid.clone()).is_ok());
        for key in [
            "guard",
            "verdict_path",
            "modifiers",
            "capture_after",
            "element_index",
        ] {
            let mut input = valid.clone();
            input[key] = json!(true);
            assert!(serde_json::from_value::<Click>(input).is_err());
        }
    }

    #[test]
    fn idle_click_revocation_is_irreversible() {
        let revoke = Revocation::default();
        assert!(!revoke.is_revoked());
        revoke.revoke();
        revoke.revoke();
        assert!(revoke.is_revoked());
    }
}
