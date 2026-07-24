# incus-mac — Design & Implementation Plan

## Context

Incus runs only on Linux, so using it from macOS requires a Linux VM running `incusd`.
Today the closest option is `colima --runtime incus`, which auto-provisions a Lima VM and
installs Incus — but Colima is shaped around Docker semantics and inherits two specific
pain points: instances aren't directly reachable from the host, and bind mounts are awkward.

`incus-mac` is a macOS CLI that boots and manages a Linux VM running `incusd`, presented
**Incus-first**: after `incus-mac start`, the user works with the stock `incus` client
against a fully configured remote. incus-mac owns only what Incus itself cannot know about
— the VM lifecycle, the host↔guest network route, host directory sharing, and TLS trust.

Greenfield: `/home/trog/code/incus-mac` is empty and not yet a git repository.

## Decisions locked during design

| Decision | Choice | Rationale |
|---|---|---|
| Instance types (v1) | **Containers only** | No nested-virt requirement, so every Apple Silicon and Intel Mac is supported. Incus VMs documented as a later phase. |
| VM engine | **vfkit** (`crc-org/vfkit`) | Small signed binary over Virtualization.framework; production-proven in podman/minikube/crc. We own the guest; the hypervisor stays a small replaceable component. |
| Networking | **Routable instance IPs** | Host route to the `incusbr0` subnet via the VM. `ssh`/`curl` a container IP directly — fixes the Colima reachability complaint. |
| CLI surface | **Lifecycle + Mac-boundary only** | Never wraps `incus launch`/`exec`/`list`. Keeps upstream Incus front and centre. |
| Storage pool | **btrfs** on a dedicated disk | In-tree on every kernel (no DKMS), retains snapshots/clones/quotas. |
| Guest base | **Debian stable + zabbly repo** | Canonical source for current `incusd`; small, predictable, long-lived cloud images. |
| Language | **Rust** (revised from Go) | Matches the rest of the author's codebase, which dominates long-term maintainability. The Go rationale did not survive scrutiny — see below. |
| Config format | **TOML** | `serde_yaml` is deprecated; the `toml` crate is actively maintained and idiomatic for Rust CLIs. Cloud-init user-data is still emitted as YAML, as cloud-init requires. |
| Crypto deps | **Pure Rust only** | `ring`/`aws-lc-rs` ship C/asm that cannot cross-compile to macOS with the host `cc`. Constraining to RustCrypto keeps `cargo check --target aarch64-apple-darwin` working from Linux. |

Verified during design:
- Apple nested virtualization requires **macOS 15+ and M3+ (FEAT_NV2)** — a hardware floor independent of engine choice.
- The M4 nested-virt problem is **Lima-specific** ([lima#4498](https://github.com/lima-vm/lima/issues/4498)); vfkit shipped `--nested` in June 2025 and podman enables it by default on capable hardware. The later VM phase is therefore unblocked by this engine choice.
- vfkit supports `--cloud-init user-data,meta-data` natively, so **no seed ISO is needed**.

## Architecture

Language: **Rust** (edition 2024), single binary via `clap`.

The original Go rationale was "matches the vfkit and Incus ecosystems". On inspection that
argument dissolves: **vfkit is invoked as a subprocess**, not linked as a library; the
**Incus client need is tiny** because the CLI is lifecycle-only and provisioning happens
in-guest via `incus admin init --preseed`; and the host side of vfkit's vsock is an
ordinary **Unix socket**, needing no special crate. What remains is the author's own
long-term maintainability, which favours Rust.

```
src/main.rs        CLI entrypoint (clap): start, stop, status, delete, mount, unmount
src/lib.rs         Module declarations; keeps main.rs a thin shell over testable units
src/config.rs      ~/.incus-mac/config.toml — cpus, memory, disk sizes, mounts, subnets
src/image/         Download + SHA512-verify + cache Debian genericcloud .raw; lease parsing
src/pki.rs         Generate/persist client + server TLS keypairs (p256 + x509-cert)
src/provision.rs   Render cloud-init user-data / meta-data
src/vm.rs          vfkit lifecycle: argument construction, start/stop, state, IP discovery
src/netroute.rs    Host route add/remove + subnet conflict detection
src/remote.rs      Configure the stock incus client's remote entry
src/mount.rs       virtio-fs share management and guest fstab wiring
```

Each module is independently testable: `provision`, `vm` (arg building), `netroute`
(route computation), and `image` (checksum/lease parsing) are pure functions over inputs,
with process execution and filesystem access behind narrow traits.

### Cross-platform development constraint

Development happens on **Linux**; the target is **macOS**. Pure logic is unit-testable
here with `cargo test`, and `cargo check --target aarch64-apple-darwin` type-checks the
real target (verified working with a pure-Rust dependency set). Anything that *executes*
vfkit, installs routes, or reads a live `/var/db/dhcpd_leases` requires a Mac, so all of
it sits behind traits with fake implementations for tests. Producing a linked macOS
binary requires a Mac or a macOS CI runner; adding `zig`/`cargo-zigbuild` later would
enable cross-linking from Linux if that becomes worthwhile.

### Boot and provisioning flow

1. **Image** — fetch `debian-13-genericcloud-arm64.raw` (or `amd64`) from `cloud.debian.org`,
   verify against `SHA512SUMS`, cache under `~/.incus-mac/images/`. Debian publishes `.raw`
   directly, so no `qemu-img` dependency. Copy-on-write clone to the instance disk and
   `truncate` to the configured size; cloud-init's `growpart`/`resizefs` grows it on boot.
2. **Pool disk** — create a second sparse raw file as a dedicated block device for the btrfs pool.
   Keeping it separate from the root disk lets the pool be resized or reset independently.
3. **PKI** — on first run generate a client keypair and a server keypair, persisted in
   `~/.incus-mac/pki/`. Both public certs are injected via cloud-init, so `incusd` trusts our
   client and presents a cert we already pin. This avoids any trust-token exchange at runtime.
4. **cloud-init** — rendered `user-data` installs the zabbly repo
   (`https://pkgs.zabbly.com/incus/stable`, key from `https://pkgs.zabbly.com/key.asc`),
   installs `incus`, then runs `incus admin init --preseed` to create the btrfs pool on the
   second disk, an `incusbr0` bridge on a **pinned, deterministic subnet**, set
   `core.https_address`, and add the injected client cert to the trust store.
5. **Launch** — vfkit invoked with a stable generated MAC:
   ```
   vfkit --cpus N --memory M \
     --bootloader efi,variable-store=~/.incus-mac/efi-store,create \
     --device virtio-blk,path=root.raw \
     --device virtio-blk,path=pool.raw \
     --device virtio-net,nat,mac=<stable> \
     --device virtio-fs,sharedDir=<host dir>,mountTag=<tag> \
     --device virtio-vsock,port=5,socketURL=~/.incus-mac/ready.sock \
     --cloud-init user-data,meta-data \
     --restful-uri tcp://localhost:<port>
   ```
6. **IP discovery** — resolve the guest address by matching our MAC in
   `/var/db/dhcpd_leases`. Readiness is signalled over **vsock**, not by polling DHCP, so
   startup doesn't race the lease file.
7. **Route** — install the host route to the container subnet:
   `route -n add -net <incusbr0 subnet> <vm ip>`. Requires privilege; v1 prompts for `sudo`
   and documents it. Routes do not survive host reboot, so `start` always reconciles.
8. **Remote** — write/refresh the `incus-mac` remote in the stock client config
   (`~/.config/incus/config.yml`) pointing at `https://<vm ip>:8443`, and optionally set it default.

### Host directory sharing

`incus-mac mount <host-path> [name]` adds a virtio-fs share, mounted in the guest under
`/mnt/mac/<tag>` via cloud-init fstab. The user then attaches it to an instance with a
normal `incus config device add ... disk source=/mnt/mac/<tag>`. incus-mac manages the
host↔guest half only; the guest↔container half stays plain Incus.

### Error handling

Fail loudly and early, with the remediation in the message: missing `vfkit` binary,
image checksum mismatch, subnet collision with an existing host route, `sudo` declined,
insufficient disk space, and boot/provision timeout (surface the guest console log path).
`start` is idempotent and reconciles state rather than assuming a clean slate; a
half-provisioned VM is torn down and rebuilt rather than patched.

### Testing

- **Unit** — cloud-init rendering (golden files), vfkit argument construction, `dhcpd_leases`
  parsing, subnet conflict detection, config defaulting/validation. These cover the logic that
  actually changes, need no VM, and run on Linux.
- **Integration** — an opt-in suite (`#[ignore]`, run with `cargo test -- --ignored`) that
  boots a real VM, waits for readiness, launches a container, and asserts the host can reach
  its IP directly. Gated because it requires a Mac.

## Known risks

1. **virtio-fs + unprivileged containers.** Shared files surface with a fixed uid/gid.
   Unprivileged containers need `shift=true` (idmapped mounts) on the disk device, and
   idmapped-mount support over virtio-fs is the least certain part of this design. Fallback
   is `raw.idmap` per instance. Prototype this before committing to the `mount` UX.
2. **Privileged route installation.** `sudo` on every `start` is friction. A launchd helper
   would remove it but adds an install/uninstall surface — deliberately deferred past v1.
3. **Subnet collisions.** A pinned `incusbr0` subnet can conflict with corporate VPNs.
   Detect at `start` and make the subnet configurable.

## Later phases (documented, not built)

- **Incus VMs via nested virtualization** — add vfkit `--nested`, gate on macOS 15+ / M3+
  with a clear diagnostic on unsupported hardware. The guest image and provisioning logic
  carry over unchanged.
- **launchd helper** for privilege-free route management and start-at-login.
- **Direct Virtualization.framework** if vfkit is ever outgrown; guest-side work is unaffected.

## Verification

1. `cargo test` — unit suite green; `cargo check --target aarch64-apple-darwin` clean.
2. `incus-mac start` from a clean `~/.incus-mac` — completes without manual intervention
   beyond the `sudo` route prompt.
3. `incus remote list` shows the `incus-mac` remote; `incus list` succeeds against it.
4. `incus launch images:debian/13 test` then `incus list` — note the container IP.
5. **The core acceptance test:** `ping <container-ip>` and `curl` a service on it *directly
   from macOS*, with no port forwarding configured. This is the Colima gap being closed.
6. `incus-mac mount ~/code`, attach it to `test`, and confirm read/write from inside the
   container.
7. `incus-mac stop` then `start` — remote reconnects and the route is reinstated.
8. `incus-mac delete` — VM, disks, host route, and remote entry all removed; verify with
   `netstat -rn` and `incus remote list`.

## Implementation order

The plan originally called for proving the riskiest assumption first via a hand-rolled vfkit
boot spike. **That spike cannot run on this Linux development machine.** It remains the first
task to perform on a Mac, and until then the vfkit invocation is validated by asserting on the
exact argument vector rather than by executing it.

Order on Linux, each via TDD:

1. `config` — defaults, validation, TOML round-trip.
2. `vm` — vfkit argument construction (encodes boot correctness; highest value testable here).
3. `image::leases` — `/var/db/dhcpd_leases` parsing and MAC matching.
4. `netroute` — subnet math and host-route conflict detection.
5. `provision` — cloud-init rendering against golden files.
6. `pki` — cert generation with p256 + x509-cert.
7. `main` — clap wiring over the above.

Then, on a Mac: the boot spike, followed by the end-to-end verification below.
