# Opt-in idle-gated X11 single click

This controlled-fork slice is selected by the operator for implementation and
independent review. It is not an accepted upstream cross-platform contract or a
shipping claim. Normal Cua Driver behavior remains unchanged when the option is
absent.

## Problem and scope decision

An embedding host needs one background click on its own exact X11/XWayland
window while a trusted, fresh all-input idle verdict permits it. Revoking an
admission scope is not equivalent to stopping already executing input. The
native send boundary must own cancellation and any paired release.

Implement an explicit trusted-launcher policy, a one-operation native runtime,
and checked XSendEvent ButtonPress/Release on the exact allowed window. No seat
input, XTest, uinput, focus/activation, overlay, clipboard, keyboard, drag,
macro, fallback, screenshot or accessibility walk is allowed in this mode.
Missing/stale/future/present/unknown/locked signal, changed producer/away episode,
wrong target, unsupported platform/path, contention and retired operations
refuse. Cancellation is latched; an owned press receives cleanup-only release
before a terminal receipt. Event acceptance remains `unverifiable` rather than
claiming a client-visible effect.

The initial platform is Linux X11/XWayland. Pure Wayland, macOS and Windows
must refuse this option explicitly. Other platform adapters are not changed.

## Lineage

This is a genuinely smaller partial superseding slice of
[misty-step/cua#1](https://github.com/misty-step/cua/pull/1), not a competing
implementation of its keyboard/capture work. That large historical draft and
its contributor history are retained unchanged. This slice adopts none of its
unfinished foreground/focus/keyboard material.

## Acceptance and remaining holds

Build the actual candidate and exercise generated private native fixtures:
useful exact click; truthful capability/manifest and installed-consumer argv;
no idle reset, pointer/focus/sibling/core-button change; red/green pre-send
refusal; before-press and between-pair cancellation; checked delivery; immediate
busy refusal; transport close and no replay/resurrection/late events.

Independent review, merge, scoped installation and real compositor/idle sensor
proof belong to the operator. A fixture is not a physical-user return. No
production installation, display action, expanded input grants or new service
is part of implementation verification.

## Implemented interface

Set `CUA_DRIVER_IDLE_CLICK_POLICY` to an absolute private JSON file in the trusted
launcher's environment. `manifest.mcp_invocation` then returns the existing
binary with `mcp --idle-click-policy <path> --no-overlay`. Direct use of that
flag is equivalent. Duplicate `--no-overlay` is harmless; `--direct` is accepted.
Other arguments, daemon proxying, grants and broader runtime modes refuse.
An empty or malformed policy never falls back to the normal runtime.

The policy has exactly these fields:

- `schema`: integer 1
- `verdict_path`: absolute path to the producer's verdict
- `display`: explicit local Unix X display, `:N`; must match `DISPLAY`
- `producer_pid`: the launcher's trusted all-input verdict producer
- `pid`, `window_id`: the one initiative-owned target
- `max_age_ms`: integer 1 through 15000

Explicit `XAUTHORITY` is required. There is no display/bus/compositor discovery.
Policy and verdict must be uid-owned private regular files (no final symlink,
at most 16 KiB). These constraints do not sandbox arbitrary code running as the
same uid: the launcher and producer are trusted, not model-supplied attestations.

The schema-1 verdict must contain matching `producer_pid`,
`input_mode: "all-input"`, finite `at` and positive finite `since` no later than
`at`, `present: false`, `drivable: true`, `reasons: ["idle"]`, `degraded: []`, and
`evidence` with `locked: false`, `meeting: false`, `idle_seconds >= 600`.
Future/stale/backwards publications refuse. The producer and target's Linux
process start identities must remain live and unchanged; a changed away
`since` value revokes the executing operation. There is no acting-window discount
or persistent operation ledger.

The guarded MCP tools are `click`, `start_session`, `end_session`, `list_windows`
and `get_window_state`. Their discovery advertises
`input.pointer.click.x11_send_event_idle_v1`. `click` accepts exact integer
`pid`, `window_id`, `x`, `y`; optional `button` (left/middle/right), `count: 1`,
`delivery_mode: "background"` and advisory `session`. Background omission is
intentional compatibility with embedding consumers that omit this property.
Modifiers, element addressing, capture-after and other tools refuse.

Read tools disclose only the allowed target's identity/geometry and explicit
metadata-only text, never pixels or an accessibility walk. An embedding consumer
may arm its exact sticky target using its existing non-vision capture/selector
API; no window is focused or raised. Do not interpret metadata as a screenshot
or visible-effect proof. Capture-after and vision capture remain unavailable.

Only one attempted click may execute per process. Busy and retired calls refuse
instead of queueing. A new process is a new explicit launcher operation: the
embedding host must retire the backend after any attempt and must not reconnect,
rearm or replay an uncertain mutation automatically. Unrelated normal sessions
must not inherit this mode or policy accidentally.

## Native send and cancellation boundary

XRes 1.2 verifies the actual local X client PID, rather than trusting spoofable
`_NET_WM_PID`. Mapped reachability, geometry, process identity and the fresh
verdict are rechecked after authorization and X11 resolution waits, at the final
press boundary. Short X11 GrabServer/UngrabServer transactions make target
checks/send atomic against XID destruction/reuse. This briefly serializes all X
clients; it is not a seat grab and never spans the 35 ms paired-click interval.
This server-wide serialization is an explicit review/deployment consideration.

Request-id-matched `notifications/cancelled`, `end_session`, EOF and SIGTERM/SIGINT
reach the executing worker without the ordinary serial input queue. The worker
also polls the verdict during the pair interval. Revocation is latched; only an
already possibly committed press permits a cleanup-only ButtonRelease to its
original, non-destroyed XID. Both sends are checked and fenced before completion.
Destroyed/reused targets and connection failures report uncertainty, never a
release to a substitute target or a replay. Nonblocking stdin prevents shutdown
from waiting on an uncancellable background read.

Terminal action results carry `isError`, `structuredContent.code`, `effect`,
`press_sent`, `release_sent`, `cleanup_complete`, `retired` and the concrete
`path: "x11_send_event"` / `route: "synthetic_events"`.
`press_sent` conservatively includes a send that may have committed before a
transport error; worker panic reports unknown send fields and incomplete cleanup.
`idle_click_complete` remains `effect: "unverifiable"`; cancellation after press
is partial, and pre-input refusal is refused. `end_session` carries the actual
terminal cleanup outcome, not a blanket successful cleanup acknowledgement.
These are server-delivery fences: an application can still process its already
delivered event later. They do not attest a visible client effect or a bounded
cleanup time when the X server itself is stalled/unreachable. SIGKILL is not
cooperative cleanup and must not be presented as such.

## Reproducing generated native contracts

From `libs/cua-driver/rust`, build with
`CARGO_BUILD_JOBS=2 cargo build -p cua-driver --offline --locked`, then run
`python3 ../tests/idle-click/run.py target/debug/cua-driver <new-managed-scratch-directory>`.
The runner requires bwrap, Xvfb, dbus-daemon, cc and pkg-config development
libraries x11, xscrnsaver, x11-xcb and xcb. It creates a device-free mount/PID/
network namespace, a cookie-authenticated private Xvfb and a private session bus.
It never uses an inherited host display, compositor socket, bus or input device.
`--consumer-export <directory>` additionally exercises supplied public embedding
consumer helpers through actual MCP I/O with a generated wire/config adapter;
that is not the complete installed embedding runtime or plugin path.

The suite checks useful/reversible clicks, before-press and between-pair return,
cancellation, contention, stale/malformed/locked/unmapped/wrong-target refusal,
source loss, policy file restrictions, XID reuse, EOF/signals/output loss,
core idle, pointer/focus/sibling isolation and no later sends/fixture events.
Pure validation/revocation tests run with
`CARGO_BUILD_JOBS=2 cargo test -p cua-driver --offline --locked idle_click`.
The normal cross-platform desktop certification and real XWayland/Hyprland
fresh-sensor proof remain required before readiness/deployment; this draft's
private fixture results do not replace those gates.
