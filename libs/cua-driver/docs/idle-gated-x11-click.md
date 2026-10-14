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

The implementation and exact interface will be recorded in the linked draft
pull request and this document before its integration consumer is bound.
