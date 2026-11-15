# Plugover as a universal device-forwarding companion: architecture and roadmap

Status: proposal, nothing implemented. Written 2026-11-14 against Plugover 0.6.0 (`ae19b98`).

## 1. Summary of decisions

1. **The core stays as it is.** Relay, host daemon, pairing, encryption and USB/IP attach do not
   know about any streaming software today except in two places: the boolean `session` flag the
   punktfunk plugin sets, and setup's punktfunk detection. Both get generalized; nothing is
   rewritten.
2. **"A stream is running" becomes a set of session sources with leases**, not one boolean. Each
   integration (punktfunk, the GameStream adapter, a CLI call, a log watcher) holds its own lease.
   Devices get a per-rule mode: `session`, `always` or `manual`.
3. **One adapter covers every Sunshine-family host** (see Phase 2 for why it is a prep-command
   hook and not the poll described here). Originally: every host
   Moonlight can talk to must answer `GET /serverinfo` on its HTTP port, and Sunshine reports the
   running app there (`currentgame`, `state`). That plus Sunshine's `global_prep_cmd` hook (do/undo)
   gives a poll signal and a push signal without per-fork code. Fork differences live in a data
   table (install paths, config file, service name, quirks), not in separate plugins.
4. **Dedicated adapters only where the platform forces it:** punktfunk (has a plugin system, kept
   as is), Wolf (sessions live in containers, devices must reach the right one), Parsec and Steam
   Remote Play (no GameStream, only log or system-state signals; experimental).
5. **Moonlight clients and forks need no integration.** The few that forward USB themselves
   (Moonlight V+, Nightjar, MoonlightVibe, Moonlight Harmony) need coexistence checks, not adapters.
6. **Native Bluetooth is HID-level forwarding, not radio forwarding.** The relay pairs devices
   with BlueZ, reads and writes them through `hidraw`, and sends HID reports over a new encrypted
   channel whose messages map one to one onto Linux `uhid`. A Linux host recreates the device with
   `uhid` on `BUS_BLUETOOTH`, so the kernel and Steam see a real Bluetooth controller with no
   translation. A Windows host presents it as a synthesized USB HID device over the usbip-win2
   driver Plugover already installs, with per-device translation profiles where Windows drivers
   expect the USB report format (DualSense, DualShock 4, Switch Pro, Xbox).
7. **A standalone UI** served by the host daemon on loopback, reusing the existing React page, so
   people without punktfunk get the same experience.
8. **Compatibility is claimed only after a test on real hardware**, recorded in the README tables,
   as today.

## 2. What exists today

| Area | Where | Notes for this work |
|---|---|---|
| Relay: lends devices, pairing, snapshots | [src/relay.rs](../src/relay.rs) | Exclusive lending (`lent_to`) already arbitrates between several hosts. |
| Host daemon: watches relays, reconciles attachments | [src/hostd.rs](../src/hostd.rs) | `session: AtomicBool` gates auto-connect; `Request::Session { active }` sets it. Recovery after drops and mode-switching wheels (`Track::follows`) is already here. |
| Wire protocol | [src/proto.rs](../src/proto.rs) | Four-byte channel magics (`PLG1/2`, `PLC1/2`, `PLP2`). Host falls back from encrypted to old magics; the same pattern works for new channels. |
| Host attach backends | [src/vhci.rs](../src/vhci.rs), [src/sysfs.rs](../src/sysfs.rs) | Linux: kernel `vhci-hcd` with socket hand-over. Windows: usbip-win2 attached to a loopback proxy, which is also the hook for synthesized devices later. |
| Config | [src/state.rs](../src/state.rs) | `AutoRule { relay, device, title, port }`. Adding optional fields with serde defaults keeps old files valid. |
| API types shared with the UI | [src/api.rs](../src/api.rs) | `Status.session: bool` is the one punktfunk-shaped field. |
| punktfunk plugin | [plugin/src/index.ts](../plugin/src/index.ts) | Pushes `stream.started`/`stopped`, resyncs from `GET /status` every 10 s, serves the console page. |
| Setup and detection | [src/setup/linux.rs](../src/setup/linux.rs), [src/setup/windows.rs](../src/setup/windows.rs), [src/setup/mod.rs](../src/setup/mod.rs) | `Found.punktfunk: bool` drives the role guess and the plugin step; detection is a probe of `punktfunk-host --version` or its install path. |
| Device identity | [src/catalog.rs](../src/catalog.rs) | `Kind::Bluetooth` exists for USB dongles, and dongles are not blocked, so dongle passthrough should already work (untested). |

Coupling to punktfunk, complete list: the plugin, `Found.punktfunk` and the plugin install and
remove steps, joining the `punktfunk` group (its udev rule makes `vhci_hcd/attach` writable), and
user-facing strings. Everything else is platform independent already.

## 3. Ecosystem findings

Checked on 2026-11-14 from repositories, docs and, where marked, source code. "Base" is what the
project says it derives from. Nothing below has been tested with Plugover.

### 3.1 Hosts

| Host | Exists, maintained | Base | Host OS | Session signal available | Own USB forwarding | Plan |
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

### 3.2 Clients

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

## 4. Architecture

### 4.1 Layers

```
            ┌────────────── host machine ──────────────────────────────────┐
 relay ───► │ core: relay links, attach backends (vhci / usbip-win2 / uhid)│
 (USB, BT)  │       reconcile loop, rules, history                         │
            │            ▲ session leases                                  │
            │ integrations: punktfunk plugin │ GameStream │ Wolf │ CLI    │
            │               Parsec/Steam watchers (experimental)           │
            │ UI: punktfunk console page │ standalone loopback page │ CLI │
            └──────────────────────────────────────────────────────────────┘
```

Rule: integrations only produce session leases and, for Wolf, device placement hints. They never
attach or detach devices themselves. The core never imports integration code; integrations talk to
it through the same `Request` API the CLI uses.

### 4.2 Session model

Replace `session: AtomicBool` with a lease table in `Daemon`:

```rust
struct Lease { source: String, client: Option<String>, until: Option<Instant> }
sessions: Mutex<HashMap<String /* source id */, Lease>>
```

- `Request::Session { active, source, client, ttl }`, all new fields `#[serde(default)]`. A missing
  `source` means `"punktfunk"`, so today's plugin keeps working against a new daemon. A new plugin
  talking to an old daemon is also fine: serde ignores unknown fields on that variant.
- Push sources (plugin, prep commands, CLI) may send `ttl`; pollers refresh their own lease.
  A lease that expires counts as ended, which covers a crashed host app that never ran its undo.
- "Session active" = any unexpired lease. Hand-detached devices stay held until all leases end,
  as `held` does today.
- `AutoRule` gains `mode: session | always | manual` (default `session`, matching today) and
  optional `sources: Vec<String>` to scope a rule ("only while Sunshine streams"). `always`
  attaches whenever the relay is online. `manual` is today's `attach`/`detach`.
- `api::Status.session: bool` stays for the current UI and gains `sessions: Vec<SessionStatus>`.

This is the only change the core needs for "standalone operation", and it is small: the reconcile
loop already does the rest.

### 4.3 The integration layer

One module, `src/integrations/`, with one file per adapter kind and a data table:

| Adapter | Signal | Hosts |
|---|---|---|
| `punktfunk` | plugin pushes (unchanged), install/remove moves here from setup | punktfunk |
| `gamestream` | poll `http://127.0.0.1:<port>/serverinfo` every 2 s for `currentgame != 0`; optional push through `global_prep_cmd` (`plugover session start --source sunshine` / `stop`) | Sunshine and every fork, Moonshine, Hermes, Polaris, SolarFlare, Rubylight (to verify) |
| `wolf` | Wolf API on its Unix socket: session start/stop events with the client and container | Wolf |
| `parsec` | tail `log.txt`, track connected users, confirm the host process is alive | Parsec (experimental) |
| `steam` | Linux: presence of the `steam-streaming-playback` PipeWire sink; Windows: `streaming_log.txt` (strings to be found) | Steam Remote Play (experimental) |

The GameStream fork table is data, roughly:

```toml
[[gamestream]]
id = "apollo"
name = "Apollo"
linux = { bins = [], units = [] }
windows = { dirs = ['%ProgramFiles%\Apollo'], service = "ApolloService" }
config = { windows = 'config\sunshine.conf' }
client_commands = true      # per-client connect/disconnect commands exist
own_usb_forwarding = false
```

Every value in that table is to be confirmed on a real install before it ships. Behavior is
decided by probing (does `/serverinfo` answer, which port, does `currentgame` change when a stream
starts) rather than by trusting the fork name, because forks look alike from outside.

Signals, and why polling is the default:

- `currentgame` means "an app is running", not "a client is connected". A Sunshine app can keep
  running after the client leaves. That matches what people want for a wheel (keep it while the
  game runs) and is reliable across forks. Apollo's pause commands can tighten it later.
- `global_prep_cmd` only applies to apps that do not set `exclude-global-prep-cmd`, does not get
  client variables, and needs a host app restart after editing. It is the fast path (no 2 s poll
  delay), not the only one.
- Polling `/serverinfo` on loopback is unauthenticated and needs no stored credentials. The REST
  API would need the admin password, which Plugover should never store.

### 4.4 Detection and guided setup

`setup::Found.punktfunk: bool` becomes `Found.apps: Vec<Detected { id, name, version, how }>`.
Detection per OS: binaries on `PATH`, systemd system and user units, Flatpak ids
(`dev.lizardbyte.app.Sunshine`), Docker containers (`wolf`), Windows uninstall registry keys,
services and install folders, and finally a `/serverinfo` probe on 47989 for anything unknown
that speaks GameStream.

Setup then shows one line per detected app, as it does for punktfunk now:

- punktfunk: unchanged steps (plugin, group).
- GameStream hosts: "Follow streams from Apollo" (poll, nothing to change) and, as a separate
  opt-in, "Add Plugover to Apollo's global prep commands" with the exact diff, a backup of the
  config file, merging with existing entries, and a note that the host app needs a restart.
  Flatpak Sunshine needs `flatpak-spawn --host plugover …` in the command.
- Wolf: mount instructions or an automatic compose snippet, shown, not applied silently.
- Parsec, Steam: "experimental" lines, off unless chosen.
- Nothing detected: host role still installs; rules default to `always` and the standalone UI is
  the place to manage it.

The role guess (`found.punktfunk || found.host`) becomes "any streaming host detected".

### 4.5 Standalone UI

The console page (`plugin/ui`) already talks to the daemon through `/api/<op>`. Serve the same
built page from the host daemon on `127.0.0.1` with a per-start token, opened by `plugover ui`.
That needs a minimal HTTP responder for static files and JSON POST (standard library is enough).
The punktfunk page keeps working through the plugin. One UI codebase, two hosts for it.

### 4.6 Coexistence with hosts that forward USB themselves

Foundation Sunshine, Rubylight and the client forks attach through the same usbip-win2 driver on
Windows. Physical devices cannot be in two places, so real conflicts are limited to driver
versions and vhci port exhaustion. `plugover doctor` reports when such a feature is enabled and
lists foreign attachments, so people know which tool owns which device. Plugover only ever
detaches attachments it made, which it already tracks by slot.

### 4.7 Multiple environments

- **Several streaming apps on one host**: the lease table unions them; `sources` scopes rules.
- **Several hosts sharing one relay**: the relay already lends each device to one host at a time.
  Add an explicit "take over" request on the relay (a host asks; the relay releases the other
  host's lease after telling it), so switching PCs does not require walking to the other one.
  Default stays first come, first served.
- **Several clients on one host (Wolf)**: devices follow the session the rule names; the Wolf
  adapter tells the core which container gets which device. This is the one place where an
  integration influences placement, and the reason Wolf gets its own adapter.

### 4.8 Reliability and recovery

Already present: relay reconnects with mDNS rediscovery, re-attach after drops, following wheels
that change product id. Additions, in order of value:

1. Match the follow logic on USB serial number when the device has one (the existing `ponytail:`
   note in `hostd.rs`).
2. Relay-side recovery for a device stuck in the export state: unbind, `authorized` toggle,
   rebind, with a bounded retry count, surfaced in history.
3. Lease expiry (4.2) so a crashed streaming app never pins devices forever.
4. Bluetooth reconnection handled by the same reconcile loop (4.9): a device that comes back is
   just another snapshot entry.

### 4.9 Native Bluetooth

#### Goals and non-goals

Goals: pair controllers and other HID devices directly with the relay, forward them to Linux and
Windows hosts with rumble, LEDs, motion sensors, touchpads, adaptive triggers and vendor feature
reports, reconnect automatically, and keep latency at one network round trip over native.

Non-goals: forwarding the Bluetooth radio itself (only possible by passing a dongle through, which
stays available as USB passthrough); Bluetooth audio (A2DP/HFP); DualSense voice-coil "HD
haptics", which over USB are an audio stream and over Bluetooth are not exposed to the HID layer
in a way Plugover can forward (rumble emulation still works).

#### Why HID level

| Option | Verdict |
|---|---|
| Pass a USB Bluetooth dongle through | Works with no new code, but the host's Bluetooth stack does everything, the radio sits on the relay only by accident, and pairing must happen from the host. Keep it as a supported extra. |
| Forward raw L2CAP/HCI | Needs Plugover to own the Bluetooth host stack on both ends. Large, fragile, no gain. |
| **Forward HID reports** | Every target device class is HID (classic HID or HID over GATT). Reports are small (≤ 78 bytes for a DualSense), the semantics are standard, and both host OSes can recreate a HID device. Chosen. |

#### Relay side (Linux first)

- **Pairing** through BlueZ over D-Bus: `Adapter1.StartDiscovery` with a filter, `Device1.Pair`,
  `Trusted = true`, `Connect`, and an `Agent1` with capability `KeyboardDisplay` so passkeys and
  confirmations reach the UI. This needs a D-Bus client; `zbus` (pure Rust, blocking API) is the
  one new dependency this plan adds. Shelling out to `bluetoothctl` is not robust enough for
  pairing prompts.
- **Data tap through `hidraw`.** BlueZ defaults to `UserspaceHID=true` for classic HID and always
  uses `uhid` for HID over GATT, so every paired HID device appears as a `hidraw` node with a
  `HIDIOCGRDESC` descriptor, `HIDIOCGRAWINFO` (bus, vendor, product), and `HIDIOCGFEATURE`,
  `HIDIOCSFEATURE`, `HIDIOCGINPUT`, `HIDIOCGOUTPUT` for report requests. One code path for BR/EDR
  and BLE.
- **Relay-local drivers are unbound** from forwarded devices (bound to `hid-generic`) so the
  relay's `hid-playstation` or `hid-nintendo` do not send their own LED or mode commands while the
  host's driver does. Same sysfs approach as `usbip-host` binding today.
- **Snapshot entries**: a Bluetooth device appears in the relay snapshot with busid
  `bt-<address>`, its vendor:product, and `transport: "bluetooth"`. Existing `AutoRule`s work
  unchanged: the address is the "port".

#### Wire protocol

A new channel, `PLH1`, encrypted from the start with the existing challenge and `tls` records
(no unencrypted variant: there are no old peers to support). Binary frames
`[type u8][len u16][payload]`, chosen to map one to one onto Linux `uhid` events:

| Frame | Direction | uhid | Relay action |
|---|---|---|---|
| `CREATE` (name, uniq, bus, vid, pid, version, country, descriptor) | relay → host | `UHID_CREATE2` | from `hidraw` info |
| `INPUT` | relay → host | `UHID_INPUT2` | each `hidraw` read |
| `OUTPUT` | host → relay | `UHID_OUTPUT` | `hidraw` write |
| `GET_REPORT` / `GET_REPORT_REPLY` (id, type, number) | both | `UHID_GET_REPORT(_REPLY)` | `HIDIOCG*` |
| `SET_REPORT` / `SET_REPORT_REPLY` | both | `UHID_SET_REPORT(_REPLY)` | `HIDIOCSFEATURE` / output |
| `GONE` | relay → host | `UHID_DESTROY` | device disconnected |

The control channel learns two things: Bluetooth devices in snapshots, and relay requests from the
host (`bt_scan`, `bt_pair`, `bt_confirm`, `bt_forget`, `take_over`). A 0.6 relay stops answering
pings when it reads a line it does not understand, so these go on a new control magic `PLC3`, with
the host falling back to `PLC2` exactly as it falls back from `PLC2` to `PLC1` today. Only `PLC3`
hosts see Bluetooth devices, so an old host never tries to USB/IP-attach a `bt-` busid.

#### Linux host

Write `CREATE` to `/dev/uhid` with the original bus (`BUS_BLUETOOTH`), vendor, product and
descriptor. The host kernel then binds the same driver it would for a locally paired controller
(`hid-playstation` checks `hdev->bus == BUS_BLUETOOTH` for report formats and CRCs, confirmed in
the source), and Steam and SDL see a Bluetooth controller through `hidraw`. No translation, full
feature parity by construction, including adaptive triggers and motion. Requires `/dev/uhid`
access: setup adds a udev rule for the Plugover user, the same way it handles `vhci_hcd` now.

#### Windows host

Windows has no user-mode `uhid`; its virtual HID framework is kernel mode and would need a signed
driver. Instead, the host daemon synthesizes a **USB HID device** and serves it on the loopback
USB/IP proxy that `vhci.rs` already uses for usbip-win2:

- Full-speed device, one HID interface, interrupt IN with `bInterval = 1`, interrupt OUT, the
  forwarded report descriptor, `GET_REPORT`/`SET_REPORT`/`SET_IDLE` on endpoint 0.
- **Generic profile** (keyboards, mice, most gamepads, joysticks): descriptor and reports passed
  through unchanged. HID is transport independent for these.
- **Translation profiles**, only for devices whose Windows drivers and games branch on transport.
  Each is a small, tested table of report-format conversions:
  - DualSense: present as USB `054c:0ce6`; input `0x31` (BT) to `0x01` (USB) by dropping the
    sequence byte and CRC; output `0x02` (USB) to `0x31` with sequence tag and CRC32 seeded with
    `0xA2`. Adaptive trigger and LED payloads are identical between the two.
  - DualShock 4: same pattern with `0x11` and calibration feature report `0x05`/`0x02`.
  - Switch Pro: answer the USB `0x80` handshake locally, then pass `0x30` reports, which are the
    same on both transports.
  - Xbox Wireless (BLE HID): first try presenting the original HID device and check whether
    Windows binds its Xbox HID driver; if not, present an Xbox 360 wired device (`045e:028e`,
    vendor class) with rumble mapped. Impulse trigger rumble may be lost in the fallback. Needs a
    real test before any claim.
- The same synthesized-USB path also works on Linux, as a fallback for software that only accepts
  USB controllers.

#### Relay on Windows

Later and experimental: read paired HID devices through the Win32 HID API (descriptor
reconstructed from preparsed data, as hidapi does), pair through WinRT `DeviceInformation.Pairing`.
Windows gives no exclusive access, so the relay's own games would also see the device. A Linux
relay (Raspberry Pi or any box) is the recommended setup.

#### Latency and radio

Bluetooth HID reports arrive every 1 to 8 ms depending on the device; Plugover adds one TCP hop
with `TCP_NODELAY` (already set by `net::tune`), so a wired LAN adds well under a millisecond.
Raspberry Pi onboard Bluetooth shares its antenna with Wi-Fi: recommend Ethernet on the relay, or a
USB Bluetooth 5 adapter. Classic Bluetooth allows seven active devices per adapter.

#### Device switching

A controller bonded to the relay stays bonded there; switching which gaming PC uses it is the
relay "take over" from 4.7, with no re-pairing. Moving it back to a console or phone means
pairing there again, which most controllers remember for one host at a time. The UI says so.

### 4.10 Security

- New channels use the existing pairing key, challenge and AES-GCM records. No new trust roots.
- Relay requests from hosts (`bt_pair`, `take_over`) are allowed only for paired hosts, and the
  relay may restrict them (`plugover share` gains a Bluetooth equivalent).
- Session leases from prep commands on Windows: Sunshine-family hosts run prep commands as the
  logged-in user, who cannot read the daemon's API token today. Add a second, narrower token that
  only permits `Session`, readable by local users, rather than loosening the main one. Worst case
  for an attacker with it: devices the owner already chose connect.
- The standalone UI listens on loopback only, with a token per daemon start.
- Prep-command edits to another app's config are shown before applying, backed up, and never made
  without consent.

## 5. Roadmap

Each phase ships on its own and keeps punktfunk behavior identical. "Done" means tested on real
hardware and recorded in the README compatibility tables.

**Phase 0: groundwork, no behavior change** (done in the working tree, not released)
- Lease table with `source` defaulting to `punktfunk` and an optional `ttl`; `Status.sessions`;
  `plugover status` and `doctor` name the sources. The hidden `plugover session` command uses the
  source `cli`.
- Tests: a 0.6 plugin request still sets punktfunk's lease; leases from two sources combine; an
  unrenewed lease ends the stream.
- Moved to Phase 2: punktfunk detection and plugin steps into `src/integrations/` and
  `Found.apps`. That code is woven into the per-OS setup (one PowerShell detection script on
  Windows), so moving it before a second integration exists would be churn without a shape to
  move it to.
- Not yet verified with a running punktfunk host.

**Phase 1: standalone operation** (done in the working tree, not released)
- Per-rule `mode = auto | always | session`. `auto`, the default, connects only during streams
  while a source reports them (one spoke within 60 s, or, for 60 s after a restart, one did
  before; sources are remembered in host.toml), and always otherwise. `manual` is just no rule
  plus `plugover attach`. Hand-attached devices stay until detached or a stream ends.
- `plugover session start|stop --source <id> [--ttl]` is public.
- `plugover ui`: the daemon serves the console page on 127.0.0.1:7277. A one-time code from the
  daemon API buys an HttpOnly, SameSite=Strict cookie; Host, Origin and Sec-Fetch-Site are checked;
  the page can only use the ops the punktfunk plugin allows.
- Moved to Phase 2: `sources` scoping on rules and the Windows session-only token, which only
  matter once a second source exists.
- A host.toml from 0.6 with rules counts punktfunk as a remembered source, so upgrading does not
  connect auto devices outside a stream while the plugin has not reported yet.
- Not yet verified: with a real relay and wheel (always mode, mode changes in the page), on
  Windows (`plugover ui` there needs administrator rights like the other host commands).

**Phase 2: Sunshine-family adapter** (done in the working tree, not released)
- Correction to 4.3, from reading each fork's `nvhttp.cpp`: over plain HTTP, Apollo, Hermes and
  Polaris always report `currentgame = 0` (only paired HTTPS clients see the game), and Vibeshine
  decides per caller. Only official and Foundation Sunshine answer it. So there is no poller; the
  shared mechanism is the `global_prep_cmd` hook, which every fork keeps (`list_prep_cmd_f`).
- Sunshine aborts the launch when a do-command fails (`process.cpp`), so the hook runs
  `plugover session start --hook`, which never fails.
- `src/sunshine.rs` finds configs by file: `~/.config/{sunshine,apollo,vibeshine,polaris,hermes}/`
  (each fork's own file name, checked in source: `polaris.conf`, `hermes.conf`, `vibeshine.conf`,
  `sunshine.conf`), the Sunshine Flatpak (`flatpak-spawn --host`), and on Windows any
  `Program Files\<app>\config\sunshine.conf` (elevated command, so it may read the daemon token).
  It merges one entry into `global_prep_cmd`, keeps a copy of the file before the first change,
  and removes the entry on uninstall.
- The hook passes the app's `port`. The daemon remembers it (`[watch]` in host.toml), counts the
  source as watching while the port answers, and ends its stream after three missed checks 5 s
  apart, for an app that crashed without running its undo command.
- Setup lists found apps, adds the hook as a planned step and announces it to the daemon; doctor
  says which configs are followed.
- Verified: unit tests for config merging; the real hook commands against a test daemon with a
  stand-in listener (start, stop, staying watched for 70 s, crash ending the stream within 17 s,
  exit 0 with the daemon down). Not verified: any real Sunshine-family app, Windows, the Flatpak.
- Not covered: Moonshine and Wolf (no prep commands), Rubylight (Rust rewrite; its API not read).

**Phase 3: reliability** (take-over done in the working tree, not released)
- Done: "take over". A new request channel (`PLR1`, encrypted after the challenge; older relays
  hang up and the host says to update) carries `take`. The relay revokes the other host's lend
  through the same path as unpairing, waits until it is back, and keeps the device for the taking
  host for 15 s (shown as that host's in snapshots, refused to others). `plugover attach --take`
  and a **Take over** button. Tested with two hosts against a real relay in `cargo test`; not with
  real hardware.
- Not done, on purpose: the serial-number guard on following. If a wheel changes its serial when it
  switches modes, the guard would stop the G29 following that works today; check on hardware first.
  Relay stuck-device recovery waits for a real report of a stuck device.
- Also from Phase 1 and 2: `sources` scoping on rules, and a narrower Windows token if elevated
  prep commands turn out not to reach the daemon.
- Serial-number following, relay stuck-device recovery, relay "take over" request, doctor checks
  for foreign USB forwarding.

**Phase 4: Bluetooth on Linux relay and Linux host** (forwarding done in the working tree)
- Done: `src/hid.rs`. The relay lists Bluetooth HID devices from sysfs (`HID_ID` bus 0005,
  `HID_UNIQ`, `report_descriptor`), lends one over `PLH1` (busid as an encrypted JSON line, tag over
  the busid, then frames that map onto uhid events), reads and writes it through hidraw, and answers
  report requests with the HIDIOC[GS]{FEATURE,INPUT,OUTPUT} ioctls. Hosts on Linux open the control
  channel as `PLC3` to see Bluetooth devices (older relays hang up; the host falls back). The host
  recreates the device with `UHID_CREATE2` on `BUS_BLUETOOTH`; detaching closes it. Lends share
  the relay's exclusivity, share list, unpairing and take-over with USB devices. Host setup adds a
  `uaccess` udev rule for /dev/uhid when needed; doctor checks it.
- Verified on this machine with a simulated Bluetooth gamepad made with uhid (a Valve vendor id, so
  the Steam udev rule let an unprivileged test relay open it): the host's copy appeared on bus 0005
  with the same ids and name, input reports streamed, an output report and a feature request reached
  the device, and detaching removed the copy. Not verified: a real controller (DualSense, DS4,
  Switch Pro, Xbox), latency, the relay's own drivers interfering, long sessions.
- Not done yet: pairing from the Plugover page or CLI (BlueZ over D-Bus, needs `zbus`); for now
  pair on the relay with `bluetoothctl`. The relay keeps its own driver bound, so a relay that is
  also a desktop sees the controller too.
- `zbus` pairing with agent, `hidraw` tap, `PLC3` and `PLH1`, `uhid` on the host, UI and CLI
  (`plugover bt scan|pair|forget`).
- Test devices: DualSense (rumble, triggers, motion, touchpad), DualShock 4, Switch Pro, Xbox
  Wireless, a BT keyboard with passkey pairing.

**Phase 5: Bluetooth to Windows hosts**
- Synthesized USB HID device on the loopback USB/IP proxy, generic profile, then the translation
  profiles one device at a time.

**Phase 6: dedicated adapters**
- Wolf (sessions and container placement), then Parsec and Steam Remote Play as experimental.
- Windows relay Bluetooth, experimental.

## 6. Verification policy

- Unit tests for what can be checked without hardware: `/serverinfo` parsing against captured
  responses from each fork, prep-command config merging, lease logic, frame encoding, translation
  profiles against captured reports (round trip and CRC).
- A real-hardware matrix per release: relay OS × host OS × host app × device, filled from actual
  runs. README tables list only those rows. Racing wheels with force feedback stay the first
  device in every matrix.
- Fork behavior is probed, not assumed: the adapter logs which signals it found, and `doctor`
  shows them.

## 7. Open questions and risks

- Does every listed fork keep `currentgame` in `/serverinfo` accurate, and on which port? Moonshine,
  Wolf and Rubylight are the most likely to differ.
- Rubylight's HTTP API and webhooks are not yet read; they may be a better signal than polling.
- Wolf: whether its API exposes session events in a stable form, and how hotplugged devices reach a
  running app container.
- Steam Remote Play on Windows: which `streaming_log.txt` lines mark start and end.
- Xbox Wireless controllers on a Windows host via synthesized USB: which driver Windows binds.
- Windows prep-command user context per fork (elevated or not, which account).
- BlueZ versions on older relay distributions, and controllers that need a vendor-specific
  pairing sequence.

## 8. Sources

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
