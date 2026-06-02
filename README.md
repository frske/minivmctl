# minivmctl

`minivmctl` is a Rust microVM control service. It starts and manages KVM-based VMs, exposes a Rocket HTTPS API, and ships with a small browser UI for local development.

## What It Does

- Creates and tracks microVMs through `VmManager`
- Exposes VM operations over HTTPS on `https://127.0.0.1:8443`
- Serves a frontend from `frontend/` for listing, creating, copying IPs, and stopping VMs
- Handles boot setup, virtio block, virtio net, MMIO routing, and guest provisioning

## Layout

- `src/main.rs` - launches the API and VM manager
- `src/api.rs` - Rocket routes and CORS
- `src/vm_manager.rs` - VM lifecycle and registry
- `src/boot.rs` - guest boot setup and Linux boot params
- `src/net.rs` - virtio-net and TAP plumbing
- `src/blk.rs` - virtio block backend
- `src/provision.rs` - host and guest provisioning
- `frontend/` - static UI
- `tests/` - boot, event, and API smoke tests
- `scripts/` - local dev and image helpers

## Prerequisites

- Rust toolchain
- KVM access on Linux
- Root privileges for provisioning and shutdown flows
- Kernel image and rootfs artifacts expected by the scripts
- Local TLS certs in `certs/` for development

## Build

```bash
cargo build
```

## Run

The repo includes `scripts/run.sh`, which:

- builds the backend
- checks for local TLS certs
- starts the Rocket API on `127.0.0.1:8443`
- starts the frontend server

```bash
./scripts/run.sh
```

## API

Base URL: `https://127.0.0.1:8443`

- `GET /` - service banner
- `GET /vms` - list running VMs
- `GET /vms/<id>` - get one VM
- `POST /vms` - create a VM
- `DELETE /vms/<id>` - request VM stop

Create payload:

```json
{
  "name": "web-1",
  "ssh_key": "ssh-ed25519 ...",
  "memory_mib": 512
}
```

Allowed VM memory is `128..=3072` MiB in `128` MiB steps.

## Tests

```bash
cargo test
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
```

There is also an API smoke test file at `tests/api.hurl`.

## Security Note

This project is intended as a local development tool. The API binds to `127.0.0.1`, and the current TLS stack has documented `cargo audit` findings that are acceptable only in this local-use context. See `SECURITY.md`.
