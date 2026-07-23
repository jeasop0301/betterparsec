# Client Keyboard Capture Filter — Evidence, Constraints & Plan

Status: **test prototype implemented; live validation and production signing remain.**
The repository now contains a KMDF keyboard class-filter prototype, LocalSystem
broker, native client integration, standalone WDK build script, and explicit
test-sign/install scripts. None of that is a shipping package or G007 evidence
until it is installed, rebooted, and exercised on the target machines.

## Observed evidence (2026-07-17)

On the tester's client machine, `WH_KEYBOARD_LL` produced zero callback events when
the client was elevated (the log records `process integrity elevated=true` and 84
re-registrations with no events). Ordinary keyboard input still reached the app
window procedure as `WM_KEYDOWN`, so ordinary in-game keys continued to work.

`RegisterHotKey(MOD_ALT, VK_TAB)` returned `0x80070581` (already registered) in the
recorded probe. This is evidence for that tested configuration, not evidence that
all Windows 11 installations behave identically. The current evidence does not
identify a particular security product, filter vendor, or mechanism. A keyboard
filter, hook-API interference, a registration conflict, and other environmental
causes remain hypotheses until the target stack and reproducible probes establish
one.

## Current fallback (no capture driver)

While immersive keyboard capture is engaged, `Ctrl+Tab` / `Ctrl+Shift+Tab` is
translated by the window procedure into a remote `Alt(+Shift)+Tab` chord
(`app-native/src/input.rs::CtrlTabRemap`). It uses application window messages
and therefore does not depend on the low-level hook. Its cost is that the host's
`Ctrl+Tab` is shadowed during immersive.

This is a useful diagnostic and operational fallback, but it does **not** capture
or suppress a locally pressed real `Alt+Tab`. It cannot satisfy G007 C5, and a
passing fallback observation cannot convert a failed or untested real-Alt+Tab cell
into a pass.

## Required architecture

The production client path is:

1. A signed keyboard **class filter** captures the client keyboard stream early
   enough to make an explicit policy decision for real `Alt+Tab`.
2. A privileged **broker service** receives the allowed capture events from the
   filter and forwards them to the client transport with authenticated,
   least-privilege IPC.
3. The client forwards the remote chord; the filter's policy suppresses the local
   chord only while an eligible immersive session is active.
4. Production install, upgrade, rollback, and uninstall must atomically manage
   both the filter and broker. Capture integrity fails closed on gaps or broker
   loss, while the short driver lease fails open for **local** keyboard
   availability so a dead broker cannot trap the workstation.

This is a client capture design. **VHF is not part of client capture:** it is an
optional host-side virtual-HID injection mechanism when the host needs it. It
cannot replace the client filter or broker that observe and suppress the locally
pressed chord.

### Implemented test path

- `drivers/betterparsec-kbdflt` is a KMDF class upper-filter prototype. Under a
  healthy 500 ms broker lease it mirrors every physical keyboard edge into one
  sequenced ring before the callback acknowledges that source. It processes
  class delivery one record at a time, so partial consumption has an exact
  prefix. A prospective Alt-down is held only on the local path: following Tab
  suppresses local Alt+Tab, while another decisive key replays Alt before that
  key. Win edges are remote-only. Ctrl+Alt+Del, Ctrl+Alt+Q/backtick and Alt+F4
  keep a local Windows path. Lease expiry, sequence/drop faults, contention and
  ring pressure latch the epoch fault and fail open locally.
- `input-broker` runs as LocalSystem, grants its single local named-pipe endpoint
  only to SYSTEM, Administrators, and the current active-console user, rejects a
  client from any other session, refreshes the lease, validates
  nonce/sequence/drop state, and disarms on every exit. This test prototype does
  not yet authenticate a particular signed BetterParsec client binary; that is
  still required for a production service.
- `app-native/src/input.rs` verifies that the pipe server is LocalSystem, waits
  for armed status, and makes the ordered broker ring the sole remote keyboard
  producer while active. Its persistent event-time router batches modifiers,
  keeps the safety chords local, forwards real Alt+Tab/Alt+Shift+Tab and Win
  chords, and releases remotely owned keys on every normal or error teardown.
  Teardown keeps the fallback producer gated while the driver fences in-flight
  callbacks, the broker acknowledges DISARM, and the UI thread drains legacy
  keyboard messages already queued by the old producer. A missing acknowledgement
  or failed release disconnects instead of enabling a second producer;
  `WH_KEYBOARD_LL` is used only when broker setup fails before ARM can take effect.
- `build-test.cmd`, `sign-test.ps1`, and `install-test.ps1` are explicit
  development-machine tools. Signing, TESTSIGNING changes, filter installation,
  and reboot are never automatic.

## Stack ordering and machine-specific risk

Keyboard class filtering is configured through Plug and Play/class registry
configuration, notably the `UpperFilters` and `LowerFilters` multi-string values
for the relevant keyboard class/device stack. Their configured ordering and the
device stack actually built by PnP determine which component observes an IRP or
service callback first; they are not selected through a universal keyboard
"altitude" value.

Consequently, a filter is not automatically a remedy for the observed hook
failure. Before relying on it, capture the target machine's actual keyboard device
stack and its `UpperFilters`/`LowerFilters` configuration, then test the signed
filter there. Another component can still transform, consume, or prevent the
events needed by this design. That is a hypothesis to test, not an attribution to
named security software.

## Signing and release contract

A production-distributed kernel driver needs the Microsoft hardware signing
release path: run the applicable HLK tests and submit the results/package for
**WHCP dashboard signing**. The release checklist must retain the resulting
signed package and hardware evidence.

Attestation signing, where Microsoft makes it applicable, is limited to
development/testing use in this plan; it is not the production release target.
Self-signed test certificates require test-signing configuration and are likewise
restricted to dedicated development/test hardware, never end-user instructions.
The repository does not yet have HLK results, WHCP dashboard signing, or a
production installer/package. The checked-in self-signed path is test-only.

## Work required before G007 closure

1. Build the checked-in WDK prototype and broker, sign the SYS on a dedicated
   test machine, install the filter/broker from an elevated shell, and reboot.
   Kernel faults can crash or lock out the machine; retain recovery access.
2. Inspect the target PnP stack and preserved `UpperFilters` ordering, then
   validate the filter with real physical keyboard input, not only synthetic
   `SendInput`.
3. Run G007 C5 with a **real Alt+Tab**, then retain the required 2-hour
   high-motion and 8-hour mixed/idle soak evidence. Until all pass, G007 remains
   BLOCK. `Ctrl+Tab` may be recorded as F1 only.
4. Before production distribution, complete the production installer lifecycle,
   applicable HLK testing, and WHCP dashboard signing. The test scripts are not
   a substitute for those release gates.

## Rejected substitutes

- **Raw Input** (`RIDEV_NOLEGACY`, usage 0x01/0x06) cannot suppress the local
  chord, so it cannot implement the real-Alt+Tab capture contract.
- **`RegisterHotKey`** is unavailable in the recorded test configuration and
  cannot be assumed available elsewhere without per-machine evidence.
- **Elevation alone** did not restore low-level-hook events in the recorded test.
- A stub, unsigned, or test-signed driver presented as an end-user feature would
  be a security and release-contract violation.
