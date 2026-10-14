//! Narrow MCP adapter: reads controls during execution; never queues input.

use crate::idle_click::{
    linux::{Guard, Revocation},
    CAPABILITY,
};
use anyhow::Result;
use cua_driver_core::{
    mcp_wire::{self, ProtocolSession},
    protocol::{initialize_result, Request, Response},
};
use serde_json::{json, Value};
use std::sync::Arc;
use std::{
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
};
use tokio::io::{unix::AsyncFd, AsyncWriteExt};

// tokio::io::stdin uses an uncancellable blocking read. That keeps runtime
// shutdown waiting forever after SIGTERM even when the owned release finished.
struct StdinLines {
    fd: AsyncFd<OwnedFd>,
    buffer: Vec<u8>,
    eof: bool,
}
impl StdinLines {
    fn new() -> io::Result<Self> {
        let fd = unsafe { libc::dup(libc::STDIN_FILENO) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            fd: AsyncFd::new(fd)?,
            buffer: Vec::new(),
            eof: false,
        })
    }
    async fn next_line(&mut self) -> io::Result<Option<String>> {
        loop {
            if let Some(end) = self.buffer.iter().position(|b| *b == b'\n') {
                let line: Vec<_> = self.buffer.drain(..=end).collect();
                return String::from_utf8(line)
                    .map(Some)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
            }
            if self.eof {
                return if self.buffer.is_empty() {
                    Ok(None)
                } else {
                    String::from_utf8(std::mem::take(&mut self.buffer))
                        .map(Some)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
                };
            }
            let mut ready = self.fd.readable().await?;
            let mut bytes = [0u8; 4096];
            match ready.try_io(|fd| {
                let size =
                    unsafe { libc::read(fd.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) };
                if size < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(size as usize)
                }
            }) {
                Ok(Ok(0)) => self.eof = true,
                Ok(Ok(size)) => {
                    self.buffer.extend_from_slice(&bytes[..size]);
                    if self.buffer.len() > 16384 {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "oversized idle-click request",
                        ));
                    }
                }
                Ok(Err(error)) => return Err(error),
                Err(_) => continue,
            }
        }
    }
}

fn refusal(code: &str) -> Value {
    json!({"isError":true,"content":[{"type":"text","text":code}],"structuredContent":{"code":code,"effect":"refused","guard_capability":CAPABILITY,"press_sent":false,"release_sent":false,"cleanup_complete":true}})
}

fn worker_uncertain() -> Value {
    json!({"isError":true,"content":[{"type":"text","text":"idle_click_worker_uncertain"}],"structuredContent":{"code":"idle_click_worker_uncertain","effect":"partial","guard_capability":CAPABILITY,"press_sent":null,"release_sent":null,"cleanup_complete":false,"retired":true}})
}

struct CancelOnDrop(Arc<Revocation>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.revoke();
    }
}

fn tool(name: &str, properties: Value, required: &[&str]) -> Value {
    json!({"name":name,"description":"Idle-gated X11 single-use mode; no seat input, pixels, accessibility walk or fallback.","inputSchema":{"type":"object","additionalProperties":false,"properties":properties,"required":required},"capabilities":[CAPABILITY],"annotations":{"readOnlyHint":matches!(name,"list_windows"|"get_window_state")}})
}

pub fn inventory() -> Value {
    json!({"capability_version":"1","tools":[
        tool("click",json!({"pid":{"type":"integer","minimum":1},"window_id":{"type":"integer","minimum":1},"x":{"type":"integer","minimum":0,"maximum":32767},"y":{"type":"integer","minimum":0,"maximum":32767},"button":{"enum":["left","middle","right"]},"count":{"const":1},"delivery_mode":{"const":"background"},"session":{"type":"string"}}),&["pid","window_id","x","y"]),
        tool("start_session",json!({"session":{"type":"string"}}),&[]),
        tool("end_session",json!({"session":{"type":"string"}}),&[]),
        tool("list_windows",json!({"session":{"type":"string"},"on_screen_only":{"type":"boolean"}}),&[]),
        tool("get_window_state",json!({"pid":{"type":"integer"},"window_id":{"type":"integer"},"session":{"type":"string"},"max_elements":{"type":"integer"}}),&["pid","window_id"])
    ]})
}

pub async fn run(guard: Arc<Guard>) -> Result<()> {
    let mut lines = StdinLines::new()?;
    let mut stdout = tokio::io::stdout();
    let mut protocol = ProtocolSession::default();
    let tools = inventory();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Value>(1);
    let mut active: Option<(Value, Arc<Revocation>)> = None;
    let mut retired = false;
    let mut closing = false;
    let mut pending_end = None;
    let mut terminal = json!({"content":[{"type":"text","text":"retired without input"}],"structuredContent":{"retired":true,"cleanup_complete":true}});
    let cancel_on_exit = Arc::new(Revocation::default());
    let _cancel_on_drop = CancelOnDrop(cancel_on_exit.clone());
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    loop {
        let response = tokio::select! {
            _ = terminate.recv(), if !closing => {
                closing=true; retired=true;
                if let Some((_,cancel))=&active { cancel.revoke(); }
                None
            }
            _ = interrupt.recv(), if !closing => {
                closing=true; retired=true;
                if let Some((_,cancel))=&active { cancel.revoke(); }
                None
            }
            result = rx.recv(), if active.is_some() => {
                active=None;
                terminal=result.as_ref().map(|r|r["result"].clone()).unwrap_or_else(worker_uncertain);
                result
            }
            line = lines.next_line(), if !closing => {
                match line? {
                    None => { closing=true; retired=true; if let Some((_,c))=&active { c.revoke(); } None }
                    Some(line) => match serde_json::from_str::<Request>(&line) {
                        Err(_) => Some(serde_json::to_value(Response::parse_error())?),
                        Ok(request) => {
                            if request.method=="notifications/cancelled" {
                                if let Some((id,c))=&active {
                                    if request.params.as_ref().and_then(|v|v.get("requestId"))==Some(id) { c.revoke(); retired=true; }
                                }
                                None
                            } else if request.is_notification() { None }
                            else {
                                let id=request.id.clone().unwrap_or(Value::Null);
                                match protocol.validate(&request) {
                                    Err(response) => Some(serde_json::to_value(response)?),
                                    Ok(era) => {
                                        let result = match request.method.as_str() {
                                            "initialize" => { let mut r=initialize_result(); r["instructions"]=json!("One trusted-policy exact X11 SendEvent-only click per process. All broader input/capture is refused. Cancellation cleans the owned pair; never replay a retired/uncertain operation."); r },
                                            "ping" => json!({}),
                                            "tools/list" => tools.clone(),
                                            "tools/call" => {
                                                let call=request.tool_call()?;
                                                if let Err(r)=mcp_wire::validate_tool_call(&request,id.clone(),era,&tools) {
                                                    // Logical refusal rather than a mutation retry surface.
                                                    let _=r;
                                                    refusal("idle_click_unsupported")
                                                } else if cua_driver_core::policy::authorize_tool_call(&call.name,&call.args).is_err() { refusal("idle_click_policy_refused") }
                                                else { match call.name.as_str() {
                                                    "click" => {
                                                        if active.is_some() { refusal("idle_click_busy") }
                                                        else if retired { refusal("idle_click_retired") }
                                                        else {
                                                            retired=true;
                                                            let cancel=cancel_on_exit.clone();
                                                            active=Some((id.clone(),cancel.clone()));
                                                            let response_id=id.clone();
                                                            let g=guard.clone(); let sender=tx.clone(); let args=call.args;
                                                            tokio::spawn(async move {
                                                                let result=tokio::task::spawn_blocking(move || g.click(args,cancel)).await.unwrap_or_else(|_|worker_uncertain());
                                                                let _=sender.send(json!({"jsonrpc":"2.0","id":response_id,"result":result})).await;
                                                            });
                                                            Value::Null
                                                        }
                                                    }
                                                    "start_session" => if retired { refusal("idle_click_retired") } else { json!({"content":[{"type":"text","text":"Idle-gated single-use target ready; no overlay"}],"structuredContent":{"active":true,"guard_capability":CAPABILITY}}) },
                                                    "end_session" => {
                                                        retired=true;
                                                        if let Some((_,cancel))=&active {
                                                            cancel.revoke();
                                                            if pending_end.is_none() {pending_end=Some(id.clone());Value::Null} else {refusal("idle_click_busy")}
                                                        } else { terminal.clone() }
                                                    }
                                                    "list_windows" | "get_window_state" => {
                                                        if active.is_some() || retired {refusal("idle_click_retired")}
                                                        else if call.name=="get_window_state" && (call.args["pid"].as_u64()!=Some(guard.policy.pid.into()) || call.args["window_id"].as_u64()!=Some(guard.policy.window_id.into())) {refusal("idle_click_wrong_target")}
                                                        else { match guard.metadata() {
                                                            Err(_) => refusal("idle_click_permission_refused"),
                                                            Ok(window) => if call.name=="list_windows" {
                                                                json!({"content":[{"type":"text","text":"Owned target metadata only"}],"structuredContent":{"windows":[window],"guard_capability":CAPABILITY}})
                                                            } else {
                                                                json!({"content":[{"type":"text","text":"Owned target metadata\nMetadata only: pixels and accessibility capture are refused in idle-click mode."}],"structuredContent":{"tree_markdown":"Metadata only: pixels and accessibility capture are refused in idle-click mode.","elements":[],"pid":guard.policy.pid,"window_id":guard.policy.window_id,"guard_capability":CAPABILITY,"metadata_only":true}})
                                                            }
                                                        }}
                                                    }
                                                    _ => refusal("idle_click_unsupported")
                                                }}
                                            }
                                            _ => refusal("idle_click_unsupported")
                                        };
                                        if result.is_null() {None} else {Some(json!({"jsonrpc":"2.0","id":id,"result":result}))}
                                    }
                                }
                            }
                        }
                    }
                }
            }
        };
        if let Some(response) = response {
            if !closing {
                stdout
                    .write_all(serde_json::to_string(&response)?.as_bytes())
                    .await?;
                stdout.write_all(b"\n").await?;
                stdout.flush().await?;
            }
        }
        if active.is_none() {
            if let Some(id) = pending_end.take() {
                if !closing {
                    stdout
                        .write_all(
                            format!("{}\n", json!({"jsonrpc":"2.0","id":id,"result":terminal}))
                                .as_bytes(),
                        )
                        .await?;
                    stdout.flush().await?;
                }
            }
            if closing {
                break;
            }
        }
    }
    Ok(())
}
