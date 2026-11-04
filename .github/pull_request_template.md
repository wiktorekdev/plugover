## What and why

## Checks

- [ ] `cargo test` (commit what it regenerates in `plugin/ui/src/generated/`)
- [ ] `cargo clippy --all-targets -- -D warnings`, also with `--target x86_64-pc-windows-gnu`
- [ ] `bun run typecheck` in `plugin/`, if the plugin changed

## Tested on

- [ ] Linux relay
- [ ] Linux host
- [ ] Windows relay
- [ ] Windows host

Say which systems and devices you used, and what you could not test.
