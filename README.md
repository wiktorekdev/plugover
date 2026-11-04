# Plugover

Plugover lets a [punktfunk](https://git.unom.io/unom/punktfunk) host use USB devices that are
plugged into a different machine: racing wheels, pedals, shifters, HOTAS, gamepads. It is built on
USB/IP, so the host gets the real USB device rather than an emulated gamepad, and force feedback and
vendor features keep working.

![Plugover page in the punktfunk console](docs/img/console.png)

**Typical setup:** you stream to a client that can't take a wheel, like a TV, and the wheel is
plugged into a small server or PC next to it. That machine becomes the **relay**; the gaming PC
running punktfunk is the **host**. Devices are connected when a stream starts and handed back when
it ends.

## Quick start

**1. On the relay** (the machine the devices are plugged into), run the installer and pick
**Relay**. It prints a pairing code at the end.

**2. On the host** (the punktfunk machine), run the same installer and pick **Host**.

Linux, as your normal user (it asks sudo for the steps that need root):

```sh
curl -fsSL https://github.com/wiktorekdev/plugover/releases/latest/download/installer.sh | sh
```

Windows, in PowerShell (it asks for administrator rights):

```powershell
irm https://github.com/wiktorekdev/plugover/releases/latest/download/installer.ps1 | iex
```

**3. Pair.** Open the punktfunk console, go to **Plugover**, pick the relay and enter the code.
Then turn on the devices you want: they connect whenever a stream runs, and turning one off gives
it back right away.

![Pairing code form](docs/img/pairing.png)

The installer shows what it will do before it does anything and installs only what is missing.

## Compatibility

| | Status |
|---|---|
| Linux relay, Linux host (x86_64) | **Tested**: Ubuntu 24.04 relay, EndeavourOS host |
| Windows relay or host | Built, not tested yet |
| Linux on ARM64 and ARMv7 (e.g. Raspberry Pi) | Built, not tested yet |
| `.deb` package | Built, not tested yet |

| Device | USB id | Status |
|---|---|---|
| Logitech G29 Driving Force | `046d:c24f` | **Tested** |

Anything that works over USB should work, but only what is listed has been tried. Hubs, USB
storage and network adapters are never shared. If Plugover names a device wrong or picks the wrong
kind, fix it in the device's details in the console or with `plugover label`. If Plugover works (or doesn't) on your setup,
[send a compatibility report](https://github.com/wiktorekdev/plugover/issues/new?template=compatibility.yml)
so it can be listed here.

## Features

- **Automatic.** Devices marked for auto-connect are plugged in when a stream starts, reconnected
  if the link drops and returned when the stream ends. With two of the same model, auto-connect
  can follow one port. A Linux relay notices plugs the moment they happen.
- **Real USB.** Force feedback and anything else the device does over USB work as they would
  locally.
- **Console plugin.** A **Plugover** page in the punktfunk console for pairing and picking
  devices.
- **Safe defaults.** Pairing with a one-time code. A relay can limit sharing to the devices you
  list or unpair a host at any time.
- **One binary** for the relay, the host daemon, the installer and the command line.

## Other ways to install

Download the binary for your system from the
[latest release](https://github.com/wiktorekdev/plugover/releases/latest) and run
`plugover setup` (on Windows, double-click the `.exe`). The installer scripts do the same after
checking the download against its SHA-256.

On Debian, Ubuntu and Raspberry Pi OS you can also install the `.deb` from the latest release
(`sudo apt install ./plugover_<version>_<arch>.deb`). It installs only the `plugover` command; run
`plugover setup` afterwards.

Without a terminal, pass the choice: `sh -s -- --relay --lan 192.168.1.0/24 --yes`, or run
`plugover setup --host --yes`. `plugover setup --uninstall` removes everything again.

`plugover update` installs the newest release and keeps pairings and settings (a package updates
through its package manager instead). Update the relay and the host together: the console and
`plugover status` warn when their versions differ.

## Troubleshooting

```sh
plugover doctor                # check this machine's setup and say what to fix
plugover logs -f               # follow the host daemon's log (--relay for the relay's)
plugover doctor --report       # everything a bug report needs, as plain text
```

Run `plugover doctor` on both machines first; it names the problem and the fix. When
[reporting a bug](https://github.com/wiktorekdev/plugover/issues/new?template=bug_report.yml),
attach `plugover doctor --report` from the relay and the host.

## Command line

Everything in the console also works from a terminal:

```sh
plugover status                # paired relays and their devices
plugover pair                  # find a relay on your network and pair with it
plugover auto 046d:c24f on     # auto-connect a device (vendor:product)
plugover auto 046d:c24f on --port 1-2   # only the one on this port
plugover attach 1-2            # connect a device now
plugover label 0fd9:0080 --name "Stream Deck" --kind other   # fix a name or kind (--reset undoes it)
plugover update                # install the newest release
```

On the relay, as root (or administrator on Windows):

```sh
plugover code                  # a new pairing code
plugover hosts                 # paired hosts
plugover unpair desktop        # remove a host; it loses its devices within a second
plugover share 046d:c24f on    # share only the devices you list
plugover share --all           # share everything again
```

Output is colored on a terminal; set `NO_COLOR=1` to turn that off.

## How it works

| Role | Machine | Runs |
|---|---|---|
| **Relay** | where the devices are plugged in | `plugover relay` (Linux kernel `usbip-host`, or usbipd-win on Windows) |
| **Host** | the punktfunk host | `plugover host` daemon + punktfunk plugin (Linux `vhci-hcd`, or usbip-win2 on Windows) |

The relay listens on TCP 7275 and announces itself over mDNS (UDP 5353). The host daemon keeps a
connection to each paired relay, hears about plugs as they happen and attaches devices when a
stream runs.

What `plugover setup` installs:

| | Linux | Windows |
|---|---|---|
| **Relay** | `/usr/local/bin/plugover`, the `plugover-relay` service, the `usbip-host` module, firewall rules for your network (firewalld, ufw or nftables) | [usbipd-win](https://github.com/dorssel/usbipd-win), a startup task, firewall rules that block usbipd's own port (TCP 3240) so pairing is the only way in |
| **Host** | `~/.local/bin/plugover`, the `plugover-host` user service, the `vhci-hcd` module, the console plugin | the [usbip-win2](https://github.com/vadimgrn/usbip-win2) driver, a startup task, the console plugin |

## Security

- A pairing code works once, for 15 minutes. It never crosses the network: both sides use it for a
  SPAKE2 key exchange, and the relay stops accepting attempts after five wrong guesses.
- Once a device is connected, its traffic is plain USB/IP and **not encrypted**. Use Plugover on a
  network you trust.

Report vulnerabilities privately, see [SECURITY.md](SECURITY.md).

## Building from source

Needs Rust with the `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`,
`armv7-unknown-linux-musleabihf` and `x86_64-pc-windows-gnu` targets, `cargo-zigbuild`, bun and
`dpkg-deb`. `cargo test` also regenerates the console's
TypeScript types in `plugin/ui/src/generated/` from `src/api.rs`; commit them with the change.

```sh
scripts/build.sh                                  # release files in dist/
scripts/deploy.sh <relay-ssh-host> <lan-cidr>     # build, set up the relay over ssh and the host here
```

| Path | What |
|---|---|
| `src/` | the `plugover` binary (relay, host daemon, CLI, `setup` installer in `src/setup/`) |
| `plugin/` | punktfunk console plugin, built into the release binaries |
| `packaging/` | `installer.sh` and `installer.ps1`, which download the binary and run setup; templates for the `.deb` package and the winget manifests |
| `data/catalog.toml` | friendly names for devices whose USB ids are unhelpful |

## Contributing

Bug reports, compatibility reports and pull requests are welcome. Questions and setup stories go
in [Discussions](https://github.com/wiktorekdev/plugover/discussions). Most of the code was written
with Claude Opus 5.5.

## License

[MIT](LICENSE)
