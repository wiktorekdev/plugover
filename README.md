# Plugover

Plugover lets a [punktfunk](https://git.unom.io/unom/punktfunk) host use USB devices that are
plugged into a different machine: racing wheels, pedals, shifters, HOTAS, gamepads. Devices are
connected over the network when a stream starts and handed back when it ends.

It is built on USB/IP, so the host gets the real USB device rather than an emulated gamepad.
Force feedback and vendor features keep working.

![USB devices page in the punktfunk console](docs/img/console.png)

**Typical setup:** you stream to a client that can't take a wheel, like a TV, and the wheel is
plugged into a small server or PC next to it. That machine becomes the relay, and the games see
the wheel as if it were plugged into the gaming PC.

> **Status:** tested with a Linux relay and a Linux host (Logitech G29). The **Windows relay is
> untested**. Most of the code was written with Claude Opus 5.5.

## Features

- **Automatic.** Devices marked for auto-connect are plugged in when a stream starts, reconnected
  if the link drops and returned when the stream ends.
- **Real USB.** Force feedback and anything else the device does over USB work as they would
  locally.
- **Console plugin.** A **USB devices** page in the punktfunk console for pairing and picking
  devices.
- **Safe defaults.** Pairing with a one-time code. Hubs and USB storage are never shared.
- **Linux and Windows** on both sides.

## How it works

| Role | Machine | Runs |
|---|---|---|
| **Relay** | where the devices are plugged in | `plugover relay` (Linux kernel `usbip-host`, or usbipd-win on Windows) |
| **Host** | the punktfunk host | `plugover host` daemon + punktfunk plugin (Linux `vhci-hcd`, or usbip-win2 on Windows) |

The relay listens on TCP 7275 and announces itself over mDNS (UDP 5353).

## Install

Files are on the [latest release](https://github.com/wiktorekdev/plugover/releases/latest).
Install the relay first: it prints a pairing code at the end.

### Relay

**Linux**: unpack `plugover-<version>-linux-x86_64.tar.gz` and run as root. `--lan` opens the
ports for your network in nftables. With another firewall, open TCP 7275 and UDP 5353 yourself.

```sh
sudo ./install.sh relay --lan 192.168.1.0/24
```

**Windows** (PowerShell): installs [usbipd-win](https://github.com/dorssel/usbipd-win) and blocks
its own port (TCP 3240) from the network, so pairing is the only way in.

```powershell
irm https://github.com/wiktorekdev/plugover/releases/latest/download/install-relay.ps1 | iex
```

### Host

**Linux**: from the same tarball, as your normal user.

```sh
./install.sh host
```

**Windows** (PowerShell): installs the usbip-win2 driver, the daemon and the console plugin.

```powershell
irm https://github.com/wiktorekdev/plugover/releases/latest/download/install.ps1 | iex
```

### Pair

Open the punktfunk console, go to **USB devices**, pick the relay and enter the code. Then turn on
**Auto-connect** for the devices you want.

![Pairing code form](docs/img/pairing.png)

Running an installer again updates Plugover and keeps existing pairings.

## Command line

Everything in the console also works from a terminal:

```sh
plugover status                # paired relays and their devices
plugover pair --code 482913    # pair with the relay on your network
plugover auto 046d:c24f on     # auto-connect a device (vendor:product)
plugover attach 1-2            # connect a device now
sudo plugover code             # on the relay: a new pairing code
```

## Security

- A pairing code works once, for 15 minutes. It never crosses the network: both sides use it for a
  SPAKE2 key exchange, and the relay stops accepting attempts after five wrong guesses.
- Once a device is connected, its traffic is plain USB/IP and **not encrypted**. Use Plugover on a
  network you trust.

Report vulnerabilities privately, see [SECURITY.md](SECURITY.md).

## Building from source

Needs Rust with the `x86_64-unknown-linux-musl` and `x86_64-pc-windows-gnu` targets,
`cargo-zigbuild` and bun.

```sh
scripts/build.sh                                  # release files in dist/
scripts/deploy.sh <relay-ssh-host> <lan-cidr>     # build, install the relay over ssh and the host here
```

| Path | What |
|---|---|
| `src/` | the `plugover` binary (relay, host daemon, CLI) |
| `plugin/` | punktfunk console plugin |
| `packaging/linux/` | `install.sh` and systemd units |
| `packaging/windows/` | host (`install.ps1`) and relay (`install-relay.ps1`) installers |
| `data/catalog.toml` | friendly names for devices whose USB ids are unhelpful |

## Contributing

Bug reports and pull requests are welcome. Please say which relay and host setup you used.
Questions and setup stories go in [Discussions](https://github.com/wiktorekdev/plugover/discussions).

## License

[MIT](LICENSE)
