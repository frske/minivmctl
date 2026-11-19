Small implementation of a VMM written in rust using rust-vmm and related components

<img width="1444" height="964" alt="high-level-overview" src="https://github.com/user-attachments/assets/ba765b5d-6f93-4f02-9d12-fc2efac77c9c" />


# minivmctl

`minivmctl` is a Rust microVM control service. It starts and manages KVM-based VMs, exposes a Rocket HTTPS API as control plane

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

## Tests

```bash
cargo test
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
```
There is also an API smoke test file at `tests/api.hurl`.

