# Plugover architecture and roadmap

Current through Plugover 0.8.0 (`9373149`). See the README for installation and the compatibility
matrix. The ecosystem survey at the end records what the original November 2026 design considered.

## 1. What ships

| Area | Implementation |
|---|---|
| USB forwarding | The relay lends each device to one paired host at a time. The host reconciles attachments and uses Linux `vhci-hcd` or usbip-win2 on Windows. An explicit **Take over** request releases a device from its previous host and reserves it briefly for the requester. |
| Stream sessions | Each source can hold a lease with an optional expiry. Devices in `auto` mode follow streams while a source is reporting them and stay connected otherwise. The other modes are `always` and `session`. Manual use is available through `attach` and `detach`. |
| punktfunk and standalone UI | The punktfunk plugin reports streams and serves the React page in its console. `plugover ui` serves the same page via a one-time login code on loopback. Linux setup can grant attach permissions independently of punktfunk. |
| Sunshine family | Setup merges a do/undo entry into each detected config's `global_prep_cmd`, keeps a backup, and removes its entry on uninstall. The commands report sessions and return success even if Plugover is unavailable. A loopback port check clears the lease if the app goes away without running undo. |
| Parsec and Steam Remote Play | Experimental log watchers detect Parsec on Windows and Steam on Linux and Windows. A detected installation only affects `auto` mode after the watcher sees a stream. |
| Bluetooth HID | Linux relays pair and read controllers through BlueZ and hidraw. Linux hosts recreate the devices with `uhid`. Windows hosts can use USB passthrough for a physical Bluetooth adapter. |
| Device recovery in 0.8.0 | A device that comes back with another product ID (a wheel switching modes) is followed only on the same relay port and within 20 seconds of dropping. Serial numbers decide when the relay reports them on both sides, otherwise the same vendor does. Linux attach tries the next free vhci port if another process took the selected one. |
| Linux gamescope routing in 0.8.0 | During a stream, the host forwards attached keyboard and mouse events into headless gamescope through libei and releases the device when the stream ends. The device setting can keep input on the desktop. Pads, wheels and joysticks continue to use their normal input paths. |
| Windows punktfunk in 0.8.0 | The plugin connects to the host daemon over a local named pipe whose access list admits the plugin's AppContainer. |

### Session and device ownership

The source name defaults to `punktfunk` for older clients. The host considers a stream active while
at least one source has a lease. Hook commands and the CLI use the same daemon request as the
plugin. On Windows the elevated Sunshine prep command uses the token-protected loopback API.
Automatic rules match a relay and model, optionally pinned to one relay port. Sources are remembered
globally to preserve `auto` behavior during a daemon restart.
[`src/hostd.rs`](../src/hostd.rs) · [`src/state.rs`](../src/state.rs) · [`src/ui.rs`](../src/ui.rs)

Sunshine's hook follows the lifetime of the app. Forks can hide the running app from unpaired
`/serverinfo` callers, so the port probe checks liveness and the prep commands provide stream
state. Linux detection looks for the native Sunshine, Apollo, Vibeshine, Polaris and Hermes config
paths and the installed Sunshine Flatpak. Windows detection looks for
`Program Files\<app>\config\sunshine.conf`. The shared hook is usable with forks that keep this
config format. Real-stream verification is tracked separately for each app and platform.
[`src/sunshine.rs`](../src/sunshine.rs) · [`src/follow.rs`](../src/follow.rs)

### Bluetooth HID on Linux

A relay exposes paired Bluetooth HID devices as `bt-<address>` entries. It lends them through
the encrypted `PLH1` channel, supporting input, output, and feature reports, and grabs accessible
evdev input nodes while the lend is active. The host creates a `BUS_BLUETOOTH` device with the
original descriptor and identifiers. The Bluetooth path shares the relay's exclusive lending,
share list, unpairing and take-over behavior with USB. Scan, pair and forget are available through
the relay CLI and the page using `PLR1`. Host setup grants access to `/dev/uhid`.
[`src/bluetooth.rs`](../src/bluetooth.rs) · [`src/hid.rs`](../src/hid.rs) · [`src/relay.rs`](../src/relay.rs) ·
[Linux UHID documentation](https://www.kernel.org/doc/html/latest/hid/uhid.html)

### Bluetooth adapter on a Windows host

Plug a USB Bluetooth adapter into the relay and attach the **adapter** to Windows through
Plugover. Pair the controller from Windows Bluetooth settings while the adapter is attached. The
radio remains beside the controller, and Windows owns the radio and the pairing. Choose **Always**
if Windows should keep the radio between streams. The relay can lend that adapter to one host at a
time. This uses the existing USB path. Adapter and controller compatibility still needs a
real-hardware run for the README matrix.
[Plugover Bluetooth instructions](../README.md#bluetooth-controllers) ·
[Microsoft's Bluetooth pairing instructions](https://support.microsoft.com/en-us/windows/pair-a-bluetooth-device-in-windows-2be7b51f-6ae9-b757-a3b9-95ee40c3e242)

## 2. Verification and reliability

Use the README for hardware claims. Test end to end with a relay, a host, and the relevant
streaming app. Record the OS versions, app versions, device and transport, along with which
features worked. Include a force-feedback wheel in the USB release checks when hardware is
available.

| Area | Evidence and remaining work |
|---|---|
| USB | The README records Linux x86_64 relay and host, Windows host, and Logitech G29 tests. Verify the 0.8.0 Windows relay and an ARM relay. Exercise take-over between real hosts and check G29 mode switches with reported serials. |
| Sunshine | A native Linux Sunshine 2026.914 stream started and stopped Plugover, and killing Sunshine ended the session through the port check. The Flatpak hook worked when run from its sandbox. The real launch stalled at the Wayland portal before reaching the hook. Verify a Flatpak stream, Windows elevation and the fork configs people use. |
| Parsec and Steam | Unit fixtures cover log parsing and truncation. A test daemon followed a hand-written Steam log. Capture actual connect, disconnect and crash behavior before marking either integration tested. Cover late-created and replaced logs, and clear stale leases when a watcher loses its log. |
| Linux Bluetooth | A same-machine test with two real DualShock 4s verified input, feature reports, rumble, light bar, and relay input grabs. The test build changed the virtual device address to avoid a local duplicate-address check. Verify unchanged identity across two machines, pairing from discovery to first input, reconnect, and additional controller models. |
| gamescope | Verified with virtual devices and punktfunk's gamescope 3.17. Verify in a real dedicated stream, including attachment, input, desktop opt-out, and cleanup after gamescope exits. Multi-user punktfunk seats remain outside this routing path. |

Automated checks cover session leases and compatibility with older plugin requests, device modes,
serial-aware reconnects, Sunshine config merging, log parsing, relay take-over, HID frame limits,
and gamescope input classification and routing. CI runs Linux tests, TypeScript checks, and
cross-target Clippy for Windows and musl. The live gamescope test is opt-in.
[CI workflow](../.github/workflows/ci.yml) · [Gamescope tests](../src/route.rs)

Gather evidence before adding aggressive USB reset recovery or foreign USB/IP ownership
diagnostics. Relay startup and failed attaches already attempt to release leftover exports.
A useful next diagnostic would identify whether a port is occupied by Plugover or another tool
on affected hosts. Keep the current slot ownership rules when investigating.
[`src/relay.rs`](../src/relay.rs) · [`src/doctor.rs`](../src/doctor.rs)

## 3. Research milestone for Bluetooth HID on Windows hosts

The synthetic USB design was dropped in November 2026 because it required per-model report
translation. Research whether Microsoft's Virtual HID Framework (VHF) can expose a controller
paired to the Linux relay as a usable virtual HID device on Windows. VHF requires a kernel-mode
source driver. Steam Input and game compatibility need a hardware test.
[Microsoft VHF overview](https://learn.microsoft.com/en-us/windows-hardware/drivers/hid/virtual-hid-framework--vhf-)

1. **Prove the host path.** Build a minimal WDK driver and verify generic HID enumeration,
   bidirectional reports, and removal. Restrict driver access to the host service. Keep relay
   networking and report validation in user space.
2. **Test a DualShock 4.** Reuse `PLH1` from the Linux relay. Check input, rumble, and feature
   reports in Windows and Steam Input, compare with direct Windows pairing, and verify teardown
   on detach, relay loss, and service restart.
3. **Decide whether to ship.** Require useful bidirectional reports without per-model
   translation, reliable cleanup, and a maintainable Microsoft-signed package that installs
   with Secure Boot enabled.
   [Microsoft driver signing options](https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/driver-signing-offerings)

## 4. Deferred integrations

- **Wolf** was dropped after a prototype. Watching its Unix socket required access changes, and
  games in Docker need hotplug support inside their own containers. Reconsider when the
  integration has a supported way to place devices in the correct container.
- **Other Sunshine-compatible servers** need a documented start/stop signal and a real test.
  Moonshine and Rubylight are candidates if their host APIs provide that signal.
- **Other USB-forwarding tools** can share the Windows vhci driver. Check coexistence on actual
  installations before identifying foreign attachments in `doctor`. Moonlight client adapters
  are outside the current scope.
- **Per-source and per-serial rules** are candidates when a real setup needs them. The current
  configuration already supports multiple session sources and optional per-port rules.

## 5. Ecosystem survey from the original proposal

Reviewed 2026-11-14 from repositories, docs and source. The table records the original proposals
and the findings at that date. Sections 1–4 describe what ultimately shipped and what is still open.

### Hosts

| Host | Exists, maintained | Base | Host OS | Session signal available | Own USB forwarding | Original proposal |
|---|---|---|---|---|---|---|
| Sunshine (SunshineStream/Sunshine, formerly LizardByte) | Yes, active | | Win, Linux, macOS | `/serverinfo` `currentgame`/`state` (read in `src/nvhttp.cpp`); `global_prep_cmd` do/undo (read in `src/config.cpp`); REST API has no session endpoint | No | GameStream adapter |
| Apollo (ClassicOldSong) | Yes | Sunshine | Windows | Same, plus per-client connect/disconnect commands and pause/resume commands | No | GameStream adapter, prefer client commands where configured |
| Vibeshine (Nonary) | Yes, very active | Sunshine | Win, Linux | Same as Sunshine (to verify) | No | GameStream adapter |
| Vibepollo (Nonary) | Yes | Apollo | Windows | Same as Apollo (to verify) | No | GameStream adapter |
| Foundation Sunshine (AlkaidLab) | Yes, active | Sunshine | Windows-focused | Same as Sunshine (to verify) | **Yes**: `reverse_tunnel_service`, off by default | GameStream adapter plus coexistence check |
| LuminalShine (NortheBridge) | Yes | Vibeshine | Windows 11 only | Same as Sunshine (to verify); indistinguishable from Sunshine over `/serverinfo` per its PR #199 | No | GameStream adapter |
| Rubylight (RamazanKara) | Yes, releases every few days | Started from Vibepollo, now a Rust rewrite | Windows | Moonlight-protocol compatible, so `/serverinfo` exists; has an HTTP API and webhooks (not yet read) | **Yes**, with its Nightjar client (protocol undocumented in the README) | GameStream adapter for detection and polling; webhooks once documented; coexistence check |
| Hermes (MrOz59) | Yes | Apollo | Linux (CachyOS/Arch tested) | Apollo lineage; to verify | No | GameStream adapter |
| Polaris (papi-ux) | Yes | Apollo | Linux only | Apollo lineage; to verify | No | GameStream adapter |
| Moonshine (hgaiser) | Yes | Own, Rust | Linux | Moonlight-compatible, `/serverinfo`; per-app compositor sessions | No | GameStream adapter (verify `currentgame`) |
| Wolf (games-on-whales) | Yes, release v2026.11 | Own, C++ | Linux, Docker | API on a Unix socket (`WOLF_SOCKET_PATH`), OpenAPI schema, events exposed via API | No | Dedicated adapter (containers) |
| SolarFlare (Koloses) | Yes | Sunshine (+PyroWave codec) | Same as Sunshine | Same as Sunshine | No | GameStream adapter |
| punktfunk | Yes | Own | Win, Linux | Plugin SDK events and `GET /status` | No | Existing plugin, unchanged behavior |
| Steam Remote Play | Yes | Valve | Win, Linux, macOS | No API. `logs/streaming_log.txt`; on Linux the `steam-streaming-playback` PipeWire sink exists only during a session | No | Experimental watcher, Linux sink first |
| Parsec | Yes | Proprietary | Host on Windows, macOS | `log.txt` lines ending `" connected."` / `"disconnected."` per user; stale after a crash unless the process is checked | No | Experimental log watcher |

### Clients

Moonlight, Artemis, Artemide, Moonlight V+, Starwire, StreamLight, Nova (Polaris's client),
Hestia (Hermes's client) and Nightjar (Rubylight's client) all exist. None needs an integration:
Plugover runs on the host and the relay, not inside the client. Notes:

- **Moonlight V+ (qiin2333)** and its desktop sibling forward USB from the client: client-side
  USB/IP server (usbipd-win, Linux usbip-host, macOS `moonlight-usbd` on usbipdcpp, Android own
  backend), TLS tunnel authenticated with the Moonlight pairing certificate plus a token from
  `GET /api/v1/usb-forwarding`, port main+7 (47996), JSON handshake
  `{"op":"forward","token":…,"busid":…}`, then raw USB/IP into Foundation Sunshine, which runs
  `usbip attach --once` against usbip-win2. **Windows hosts only; the Linux host controller returns
  unsupported.** One tunnel per busid; the client allows one at a time.
- **Lessons from it**: their PR #1083 found helper processes outliving Sunshine and a race where a
  busid is released before its fire-and-forget detach finishes. Plugover already tracks
  attachments per slot and detaches synchronously; keep it that way. Moonlight Harmony's OTG
  passthrough does not support isochronous endpoints, which matters for headsets and some wheels.
- **Starwire** carries V+ features, so it may forward USB too. **Nightjar** forwards USB to
  Rubylight. **MoonlightVibe** (with ApolloVibe) does it on Windows with usbipd-win.
- **StreamLight** pairs with a host companion (StreamTweak) through signed commands; no device
  forwarding found.
- **Moonlight-Web (moonlight-web-stream)** streams Sunshine to a browser over WebRTC. No USB,
  WebUSB or WebHID support was found; a browser cannot speak USB/IP, so the only path would be a
  WebHID bridge, which is out of scope.

What this means for Plugover: the client-embedded approach ties forwarding to one client fork and,
today, to Windows hosts. Plugover's advantages are a dedicated relay that stays up between
streams, Linux hosts, any streaming software, and kernel-side data paths. Interoperating with the
V+ tunnel (accepting it on Linux hosts) is possible later but not planned.

## 6. Survey sources

- Sunshine: [repository](https://github.com/LizardByte/Sunshine), [API docs](https://docs.lizardbyte.dev/projects/sunshine/latest/md_docs_2api.html), `src/nvhttp.cpp` and `src/config.cpp` (read 2026-11-14), [issue #2842](https://github.com/LizardByte/Sunshine/issues/2842)
- Apollo: [repository](https://github.com/ClassicOldSong/Apollo), [Client Commands](https://github.com/ClassicOldSong/Apollo/wiki/Client-Commands), [releases](https://github.com/ClassicOldSong/Apollo/releases)
- [Vibeshine](https://github.com/Nonary/vibeshine), [Vibepollo](https://github.com/Nonary/Vibepollo)
- Foundation Sunshine: [repository](https://github.com/AlkaidLab/foundation-sunshine), [PR #1034](https://github.com/AlkaidLab/foundation-sunshine/pull/1034), [PR #1083](https://github.com/AlkaidLab/foundation-sunshine/pull/1083)
- LuminalShine: [repository](https://github.com/NortheBridge/luminalshine), [PR #199](https://github.com/NortheBridge/luminalshine/pull/199)
- [Rubylight](https://github.com/RamazanKara/Rubylight), [Hermes](https://github.com/MrOz59/Hermes), [Polaris](https://github.com/papi-ux/polaris), [Moonshine](https://github.com/hgaiser/moonshine), [Wolf](https://github.com/games-on-whales/wolf) and its [API docs](https://games-on-whales.github.io/wolf/stable/dev/api.html), [SolarFlare](https://github.com/Koloses/Solarflare)
- Moonlight V+: [moonlight-qt PR #219](https://github.com/qiin2333/moonlight-qt/pull/219), [reverse tunnel design](https://github.com/qiin2333/moonlight-qt/blob/master/docs/remote-usb-reverse-tunnel.md), [V+ issue #650](https://github.com/qiin2333/moonlight-vplus/issues/650), [Moonlight Harmony PR #132](https://github.com/AlkaidLab/moonlight-harmony/pull/132)
- Other USB forwarding forks: [Darfeikus/moonlight-qt PR #2](https://github.com/Darfeikus/moonlight-qt/pull/2), [MoonlightVibe PR #27](https://github.com/vibesoftwarecoder/MoonlightVibe/pull/27)
- Clients: [Starwire](https://github.com/SicarioGaming/Starwire), [StreamLight](https://github.com/FoggyBytes/StreamLight), [Perigee (Artemide lineage)](https://github.com/XT-Martinez/perigee), [moonlight-web-stream](https://github.com/MrCreativ3001/moonlight-web-stream/releases)
- Parsec: [auto-parsec](https://github.com/Borgotto/auto-parsec/blob/main/auto-parsec.ps1), [ci-kit PR #163](https://github.com/SSFSKIM/ci-kit/pull/163)
- Steam Remote Play: [remoteplay-display](https://github.com/AstreaeLuna/remoteplay-display), [RemotePlayAutoHDR](https://github.com/michaelphagen/RemotePlayAutoHDR)
- Bluetooth: BlueZ [`profiles/input/input.conf`](https://github.com/bluez/bluez/blob/master/profiles/input/input.conf), Linux [`drivers/hid/hid-playstation.c`](https://github.com/torvalds/linux/blob/master/drivers/hid/hid-playstation.c) (both read 2026-11-14)
