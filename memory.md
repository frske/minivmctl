# Codebase Memory

## Project
- Rust 2024 crate `scratch-new` that runs a small KVM-based VMM and exposes a Rocket HTTPS API for creating, listing, and stopping VMs.
- Static frontend in `frontend/` talks to the API at `https://127.0.0.1:8443`.
- Runtime is Linux/root-dependent: uses `/dev/kvm`, TAP/TUN, bridge `br0`, iptables, mounted ext4 rootfs images, and hard-coded kernel/rootfs paths under `/home/frsk/prj/kernel_images/`.

## Entry Points
- `src/main.rs`: initializes tracing with default filter `scratch_new=info,rocket=warn`, starts `VmManager` on a background thread, launches Rocket API.
- `src/lib.rs`: exports modules `api`, `blk`, `boot`, `bus`, `event`, `mmio`, `net`, `provision`, `serial`, `virtio`, `vm_manager`.
- `scripts/dev.sh`: builds backend, checks sudo, starts backend with Rocket TLS config, and serves `frontend/` with Python HTTPS on port `5173`.
- `Rocket.toml`: binds API to `127.0.0.1:8443` with certs in `certs/`.

## API
- `src/api.rs` mounts routes at `/`:
- `GET /vms` returns sorted running VMs as `{ id, name, ip_address, memory_mib, status }`.
- `POST /vms` accepts `{ name, ssh_key, memory_mib }`, validates memory, and starts a VM through `VmManagerHandle`.
- `DELETE /vms/<id>` requests hard stop and returns a message when the manager accepts the request.
- Memory limits: 128-3072 MiB, multiple of 128.
- CORS allowlist includes localhost/127.0.0.1/0.0.0.0 frontend origins on `5173`, both HTTP and HTTPS except no `https://0.0.0.0`.
- `io::ErrorKind` maps to HTTP status: invalid input 400, not found 404, already exists 409, disconnected/broken pipe 503, timed out 504, otherwise 500.

## VM Manager
- `src/vm_manager.rs` owns all VM state and event routing. External callers use `VmManagerHandle`, which sends `Start`, `List`, and `Stop` commands over `mpsc` and wakes the manager via `EventWrapper`.
- Manager command timeouts: start 120s, list/stop 5s.
- VM ids are sequential `u8` from `next_vm_id`; guest IP is `10.10.0.{id + 2}`, TAP is `tap{id}`, MAC is `02:12:34:56:78:{id}`.
- Shared host networking is provisioned lazily before first VM and deprovisioned after last VM cleanup.
- Each VM gets virtio-net at MMIO `0xd0000000` IRQ 5 and virtio-blk at `0xd0001000` IRQ 6; serial COM1 uses IRQ 4.
- Stop uses `VcpuStopper` with `SIGUSR1` to interrupt KVM run, then cleanup happens through the vCPU exit event path.
- `VmInfo.status` is currently always `"running"` for listed instances.
- Cleanup removes `/tmp/vm-{id}` rootfs copy and TAP interface; shared cleanup removes iptables rules and bridge.

## Boot And Devices
- `src/boot.rs` builds a one-vCPU x86_64 KVM VM, allocates `GuestMemoryMmap`, loads an ELF kernel, sets long mode paging/GDT/regs, writes Linux boot params and cmdline.
- Boot cmdline includes serial console, no ACPI/APIC, root `/dev/vda`, and two virtio-mmio devices.
- `src/virtio.rs` implements descriptor chains, queue pop/push, feature-page helpers, queue validation, notifications, and `VirtioDevice` trait.
- `src/mmio.rs` implements virtio-mmio transport registers, status transitions, queue setup, feature negotiation, config access, interrupt ack, reset, and device activation when all queues are valid.
- `src/blk.rs` implements virtio block with IN/OUT/FLUSH, descriptor parsing, guest-memory copy helpers, status writes, and backing `File` I/O.
- `src/net.rs` implements TAP setup, virtio-net queues/config/features, RX/TX processing, vnet header handling, and TAP readiness registration.
- `src/bus.rs` provides a sorted range bus for MMIO/PIO devices with overlap/empty/overflow checks and base-relative read/write routing.
- `src/event.rs` provides `EventWrapper` around nonblocking eventfd plus an epoll-backed generic `EventLoop` and pre-registration support.
- `src/serial.rs` supplies no-op serial event callbacks.

## Provisioning
- `src/provision.rs` requires root for host and guest provisioning.
- Template rootfs path is hard-coded as `/home/frsk/prj/kernel_images/rootfs.ext4`.
- Per-VM rootfs lives under `/tmp/vm-{id}/rootfs.ext4`; provisioning copies the template, mounts it, writes systemd-networkd config, writes `/root/.ssh/authorized_keys`, then unmounts.
- Shared networking creates bridge `br0` at `10.10.0.1/24`, enables IPv4 forwarding, and adds NAT/FORWARD iptables rules for the default host interface.
- Per-VM host networking creates TAP `tap{id}`, sets it up, and enslaves it to `br0`.

## Frontend
- `frontend/index.html` is a static VM control page: create form, refresh button, status message, running VM table.
- `frontend/app.js` uses vanilla JS fetches to `https://127.0.0.1:8443`; stores SSH key in localStorage key `scratch.vm.sshPublicKey`; supports create, refresh, delete, and copy-IP UI.
- `frontend/style.css` is a light, table-oriented responsive layout with simple panels and form controls.

## Tests
- `cargo test` covers unit/integration tests for boot helpers, bus routing, event wrapper/loop, mmio behavior, provisioning helper functions, and device internals.
- Tests avoid full VM boot where possible; API smoke test is `tests/api.hurl` and requires a running HTTPS backend plus real root/KVM provisioning.
- Useful API test command from file: `hurl --variable base_url=https://127.0.0.1:8443 --insecure tests/api.hurl`.

## Engineering Notes
- Many runtime paths and privileges are environment-specific; avoid assuming tests can start real VMs in an unprivileged environment.
- Keep changes surgical; low-level modules contain unsafe/ioctl/KVM/virtio details where small behavioral changes can break boot.
- Prefer adding focused unit tests around helper logic rather than full integration tests unless root/KVM setup is explicitly available.
- Existing style is direct and minimal, with some older comments/TODO-like notes left in place; do not clean unrelated comments while making feature fixes.
