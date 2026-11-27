<h1>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/img/plugover-logo-dark.svg">
    <img alt="Plugover" src="docs/img/plugover-logo-light.svg" height="56">
  </picture>
</h1>

[![Release](https://img.shields.io/github/v/release/wiktorekdev/plugover?label=release&color=3f3f46&labelColor=27272a)](https://github.com/wiktorekdev/plugover/releases/latest)
[![CI](https://img.shields.io/github/actions/workflow/status/wiktorekdev/plugover/ci.yml?branch=main&label=CI&labelColor=27272a)](https://github.com/wiktorekdev/plugover/actions/workflows/ci.yml)
[![License](https://img.shields.io/github/license/wiktorekdev/plugover?color=3f3f46&labelColor=27272a)](LICENSE)

Plug a wheel, pedals or controller into the computer next to you and use it on the gaming PC you
stream from. Plugover forwards the real USB device over your network, so force feedback and vendor
software keep working. Devices connect when a stream starts and come back when it ends.

![The Plugover page](docs/img/console.png)

The **relay** is the computer (or Raspberry Pi) the devices are plugged into. The **host** is the
gaming PC.

## Install

Run on the relay first, then on the host:

```sh
curl -fsSL https://github.com/wiktorekdev/plugover/releases/latest/download/installer.sh | sh
```

```powershell
irm https://github.com/wiktorekdev/plugover/releases/latest/download/installer.ps1 | iex
```

The relay shows a six-digit code. Enter it on the host in the punktfunk console, on the page
`plugover ui` opens, or with `plugover pair`. Then turn on the devices you want.

Works with punktfunk, Sunshine and its forks, and experimentally Parsec and Steam Remote Play.
Anything else: keep devices always on, or call `plugover session start` and `stop`.

Bluetooth controllers can be paired with the relay too (Linux on both sides).

## Tested

Linux relay and host, Windows host, punktfunk, Sunshine on Linux, Logitech G29, DualShock 4.

Not tested yet: Raspberry Pi, Windows relay, Sunshine on Windows, Parsec, Steam Remote Play.
[Tell us](https://github.com/wiktorekdev/plugover/issues/new?template=compatibility.md) how it went.

## Help

`plugover doctor` on both machines says what is wrong. `plugover --help` lists every command.
Still stuck? [Open an issue](https://github.com/wiktorekdev/plugover/issues/new?template=bug_report.md)
with `plugover doctor --report` from both machines.

Security details are in [SECURITY.md](SECURITY.md).

## Building

```sh
scripts/build.sh    # release files in dist/ (Rust, cargo-zigbuild, bun, dpkg-deb)
cargo test
```

[MIT](LICENSE)
