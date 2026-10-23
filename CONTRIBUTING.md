# Contributing

Thanks for helping! Bug reports from real hardware are the most valuable contribution: please use the
issue templates, which ask for `opencue devices` output and the daemon log.

## Building

```sh
cargo build --release
cargo test
cargo fmt --all && cargo clippy --all-targets -- -D warnings   # CI runs both
```

Run the daemon in the foreground with detailed logs (stop the service first):

```sh
systemctl --user stop opencue
RUST_LOG=debug ./target/release/opencue daemon
```

Handy tools:

- `cargo run --example probe -- cc` reads a Commander Core's status, then hands it back to hardware mode.
- `cargo run --example preview -- out/` renders every LCD and Nexus screen to PNGs.
- `opencue-gui --screenshot <page> file.png` renders a GUI page off-screen (pages: dashboard, cooling,
  lighting, lcd, nexus, settings).

## Adding a device

1. Add a driver in `crates/opencue-core/src/devices/` that talks to the device through `hidraw::HidDevice`.
   Keep it to protocol only; scheduling, config and rendering live elsewhere.
2. Register its USB ids and HID interface in `devices::discover`.
3. Add a worker in `crates/opencue/src/daemon.rs` (see `run_mm700` for a small example). A worker should
   return an error on any I/O problem; the manager reopens the device with backoff.
4. Every driver needs a `restore_hardware_mode`, so the device falls back to safe built-in behaviour
   when OpenCUE stops.

Fan control must fail safe: never let a pump stop, and run at 100% when a temperature is unknown.

## Commits

[Conventional Commits](https://www.conventionalcommits.org/): `feat(gui): …`, `fix(core): …`, `docs: …`.
