Small implementation of a VMM written in rust using rust-vmm and related components

<img width="1444" height="964" alt="high-level-overview" src="https://github.com/user-attachments/assets/ba765b5d-6f93-4f02-9d12-fc2efac77c9c" />


# minivmctl

`minivmctl` is a Rust microVM control service. It starts and manages KVM-based VMs, exposes a Rocket HTTPS API as control plane

## Quick start

Needs Linux with KVM, `sudo` (TAP/bridge setup and loop mounts), Rust, and the kernel/rootfs build tools (`debootstrap`, `flex`, `bison`, `bc`, `openssl`, `libelf`).

Build an ELF kernel and an ext4 rootfs. The VMM loads the kernel directly, so `-f elf` is required; this takes a while:

```bash
sudo ./scripts/make_image_6x.sh -f elf -j "$(nproc)" -s 2G -w "$HOME/vmimg"
```

Then point the VMM at the artifacts by updating `KERNEL_PATH` in `src/vm_manager.rs` and `ROOTFS_PATH` in `src/provision.rs`:

- `$HOME/vmimg/kernel/linux-6.6.99/vmlinux`
- `$HOME/vmimg/rootfs/rootfs.ext4`

Generate the local TLS certs and start the API and frontend:

```bash
mkdir -p certs
openssl req -x509 -newkey rsa:2048 -nodes -days 365 \
  -keyout certs/localhost-key.pem -out certs/localhost.pem \
  -subj "/CN=localhost" -addext "subjectAltName=DNS:localhost,IP:127.0.0.1"

./scripts/run.sh
```

`run.sh` builds the backend and serves the API at `https://127.0.0.1:8443` and the frontend at `https://127.0.0.1:5173`.

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

