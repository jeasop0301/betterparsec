# Input Capture Below Security Filters — Feasibility & Plan

Status: **DECISION RECORD, not yet implemented.** Kernel driver is a
roadmap item, gated on signing infrastructure. This document exists so
the driver is not silently faked or half-built.

## Problem (confirmed by 2026-07-17 live evidence)

On the tester's client machine, `WH_KEYBOARD_LL` receives **zero**
callback events — even when the client runs **elevated** (log line
`process integrity elevated=true`, then 84 re-registrations with no
events). Ordinary key input still reaches the app's wndproc
(`WM_KEYDOWN`), which is why in-game keys work. This signature — LL hook
blinded, wndproc keys fine, survives elevation — is characteristic of
anti-keylogging security software (Korean banking/security suites:
AhnLab, TouchEn, nProtect and similar) that installs a keyboard filter
and specifically defeats `SetWindowsHookEx(WH_KEYBOARD_LL)` keyloggers.

`RegisterHotKey(MOD_ALT, VK_TAB)` was also empirically falsified as a
fallback: it returns `0x80070581` (hotkey already registered) on **every**
Windows 11 machine, including the clean dev box — the OS pre-registers
Alt+Tab. See `app-native/src/bin/hook_probe.rs` (`RegisterHotKey(Alt+Tab):
FAILED`).

## What actually ships today (no driver)

`Ctrl+Tab` / `Ctrl+Shift+Tab` while immersive keyboard capture is engaged
is translated in the wndproc into a full remote `Alt(+Shift)+Tab` chord
(`app-native/src/input.rs::is_wndproc_alt_tab` + `forward_alt_tab`). This
rides the app's own window messages, so **no keyboard hook and no driver
is involved** — it cannot be blocked by an LL-hook filter and works at any
privilege. Cost: the host's own `Ctrl+Tab` is shadowed during immersive.

This is the correct pragmatic answer for the special-key-forwarding use
case. A driver is only required if we want to intercept and **suppress**
the *real* `Alt+Tab` chord on a hook-blocked machine.

## Why a kernel driver is not buildable "right now"

A production keyboard capture/filter driver (kbfiltr-style upper filter on
`kbdclass`, or a Raw-Input-elevating approach) requires, in order:

1. **WDK toolchain** (Windows Driver Kit) — not present in this build
   environment; the workspace is a Rust/cargo tree with no kernel toolchain.
2. **Kernel code** in C (WDM/KMDF). Rust kernel drivers exist but are
   experimental and still need the WDK link/sign path.
3. **Driver signing** — kernel-mode drivers on x64 Windows 10/11 require
   either:
   - an **EV code-signing certificate** + **Microsoft attestation signing**
     (Partner Center) for non-WHQL, or
   - full **WHQL/HLK** certification for broad distribution.
   Neither exists for this project. A self-signed test cert only loads
   under **test signing mode** (`bcdedit /set testsigning on` + reboot),
   which is unacceptable for end users.
4. **Reboot + physical-hardware testing**. Kernel bugs BSOD the machine;
   there is no way to build, load, or verify a `.sys` in this session.

Shipping a stub `.sys` or an unsigned driver as if it were a feature would
be a lie and a security liability. It is intentionally NOT scaffolded here.

## The altitude caveat (a driver may not even fix this machine)

Filter drivers load at an **altitude** (Microsoft-allocated). If the
security suite's keyboard filter sits at a **higher altitude** than ours in
the `kbdclass` stack, it processes/scrubs scancodes **before** our filter
sees them — our driver would be blinded exactly like the hook is. Beating
it is not guaranteed by "having a driver"; it depends on relative altitude,
which is contested territory and can escalate into an anti-cheat/anti-
keylogger arms race. This must be validated on the actual target machine
before committing to the effort.

## Cheaper mechanisms evaluated and rejected

- **Raw Input keyboard** (`RIDEV_NOLEGACY`, usage 0x01/0x06): reads from
  `kbdclass` output. If the filter is a *kbdclass upper filter* it blinds
  Raw Input too; if it is *LL-hook-specific* Raw Input would work — but Raw
  Input **cannot suppress** a key, so local Alt+Tab would still fire. It
  adds nothing over the existing wndproc path for non-suppressed keys.
  Not worth the complexity for this problem.
- **`RegisterHotKey`**: falsified (0x80070581), see above.
- **Elevation alone**: falsified (hook still zero events elevated).

## Recommended sequencing

1. **Confirm `Ctrl+Tab` (07-17h) works on the target machine** before any
   driver investment. If it forwards to the host, the *functional* need is
   met without a driver.
2. **Gather evidence** the driver would even help: does Parsec's Alt+Tab
   actually reach the host on that machine, and what is `hook_probe.exe`'s
   event count (synthetic `SendInput` bypasses device-level filters — 10
   means "real keys scrubbed at device level", 0 means "hook API blocked").
   This distinguishes a device-filter (driver-beatable only by altitude)
   from an API-block (hook-specific, Ctrl+Tab already suffices).
3. **Only if 1 is insufficient and 2 shows a device-filter**, open a
   dedicated driver project with: EV cert acquisition, Partner Center
   attestation signing, a KMDF `kbdclass` upper filter, installer/service
   integration (fits the G005 supervisor + G006 packaging story), and a
   test-signing dev loop on dedicated hardware. Estimate: multi-week, cert
   lead time dominates.

## Placement

Roadmap: post-P0. The driver is a distribution/packaging concern (installer
must register the service + driver, uninstaller must remove them), so it
belongs after G007 live closure alongside the signed-installer work, not in
the reliability core.
