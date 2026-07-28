# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`malleus` is a Rust CLI (edition 2024, single binary via `clap`) that runs Incus from macOS
through a managed Linux VM booted with `vfkit`. It is deliberately **Incus-first**: it owns
only what Incus cannot know about — VM lifecycle, the host↔guest route, host directory
sharing, and TLS trust — and never wraps `incus launch`/`exec`/`list`.

`resume-design-of-incus-mac-memoized-wall.md` is the authoritative design document: locked
decisions, the full boot/provisioning flow, known risks, and the intended module layout.
Read it before making architectural changes — several current gaps are deliberate, not bugs.

## Commands

```bash
cargo build
cargo test                                    # full unit suite (lib + bin)
cargo test --lib                              # library modules only
cargo test --lib image::                      # one module's tests
cargo test a_zombie_is_not_a_running_vm       # one test by name
cargo check --target aarch64-apple-darwin     # REQUIRED before considering work done
cargo run -- --help
```

`cargo check --target aarch64-apple-darwin` is not optional. Development happens on Linux
but macOS is the only real target, so every change must type-check there.

## Two constraints that shape the whole codebase

**1. Pure-Rust dependencies only.** `ring` and `aws-lc-rs` ship C/asm that cannot cross-compile
to macOS with the host `cc`, which would break the cross-check above. Crypto is confined to
RustCrypto (`p256`, `x509-cert`, `sha2`). Do not add a dependency that pulls in a C toolchain.
There are no `dev-dependencies` either — tests hand-roll a `TempDir` (see `pki.rs` and the
`main.rs` test module) rather than pull in `tempfile`. Match that if you need one.

**2. Nothing that touches a Mac can be executed here.** vfkit, host routes, `ps`, and a live
`/var/db/dhcpd_leases` all require macOS. So logic lives in pure functions over plain data,
and I/O sits behind narrow seams:

- `vm::build_args(&VmSpec) -> Vec<String>` builds the exact vfkit argv but never runs it;
  correctness is pinned by asserting the full vector.
- `provision::render_user_data`/`render_meta_data` render cloud-init YAML from a `ProvisionSpec`.
- `leases::find_lease` / `leases::find_in_arp` parse lease-file and `arp -an` *text*.
- `image::looks_like_disk_image` inspects a byte header; `netroute::find_conflict` is subnet math.
- `main.rs` has two injected seams: the `IncusRunner` trait (process execution) and
  `vm_is_running: &dyn Fn(&Path) -> bool` (process liveness), both threaded through
  `cmd_autoconfigure_with_runner` and `resolve_vm_ip`. `cmd_autoconfigure` is the thin
  production wrapper supplying `ProcessIncusRunner` and `is_vm_running`.

When adding behaviour, keep `main.rs` a shell: put the logic in a `lib.rs` module as a pure
function, or extend a seam. Do not introduce direct `Command::new` or absolute-path I/O in a
code path tests need to reach.

## Current implementation state

- `start` downloads/creates disks (`image.rs`), generates PKI, renders cloud-init, writes
  `vfkit.args`, and **launches vfkit**, recording `vfkit.pid`. It does not install the host route.
- `autoconfigure` starts the VM if it isn't running, resolves the guest IP, copies the client
  cert/key into the Incus config dir, and adds/switches the `malleus` remote.
- `stop` signals the recorded PID; `status` reports VM/PKI state; `delete` removes the state dir.
- `mount` / `unmount` are still print-only stubs. `mount` validates the tag and constructs a
  `Share`, then discards it — nothing persists registered shares, so `ProvisionSpec.mounts` is
  always empty even though rendering fully supports it. (`VmSpec.shares` always carries exactly
  one entry, the state-dir share below.)
- `netroute` is implemented and tested but not called from anywhere.
- Design-doc modules `remote.rs` and `mount.rs` do not exist; remote wiring lives inline in
  `main.rs` as `autoconfigure`. `image.rs` fetches Debian 12/bookworm, while the design doc
  specifies Debian 13, and there is still no SHA512 verification against `SHA512SUMS`.

## Key facts spread across files

- **The state dir is itself a virtio-fs share.** `cmd_start` shares `state_dir` into the guest
  with tag `malleus-state`, mounted at `/mnt/mac/malleus-state`. The guest writes its own IP to
  `guest-ip` there, which the host reads back. So `~/.malleus` is guest-writable, and files in
  it (`vfkit.pid`, `vfkit.err`, logs) sit alongside guest-authored data.
- **Guest IP discovery tries five channels in order** (`resolve_vm_ip`), retrying every 2s up to
  60s: vfkit REST API, vsock readiness signal, the virtio-fs `guest-ip` file, `Reported IP` lines
  in `vfkit.guest.log`, then DHCP leases and the ARP table. The first four depend on guest-side
  scripts injected by `provision.rs` `runcmd`, and the vsock one additionally needs `socat`,
  which is not installed until the `apt-get install -y incus socat` step. **First boot will not
  produce an IP inside 60s.**
- **vfkit flag groups are all-or-nothing.** `--kernel`, `--initrd` and `--kernel-cmdline` must be
  set together; emitting one alone makes vfkit reject the whole argv and exit before booting.
  We boot via EFI, so none of them may appear — pinned by
  `vm::tests::efi_boot_omits_the_direct_kernel_boot_flag_group`.
- **Process liveness must not use `kill -0`.** A vfkit that dies while malleus runs stays a
  zombie until reaped and still answers signals, and a stale `vfkit.pid` can name a recycled PID.
  `is_vm_running` shells out to `ps -o stat=,comm=` and delegates to the pure
  `describes_live_vfkit`. `start_vm` also polls `try_wait()` for a 2s grace period so a rejected
  argv surfaces vfkit's own stderr instead of a silent 60s discovery timeout.
- **Downloads must be validated.** `curl` exits 0 on HTTP errors unless `--fail` is passed, so an
  error page can land under the image's name and be padded to a 20 GiB unbootable disk.
  `image::verify_disk_image` checks for an MBR/GPT signature and **deletes** anything that fails,
  because every caller skips work when the file already exists — a bad artifact left in place
  would be reused forever.
- **Trust model**: `pki::load_or_create` generates client *and* server keypairs on first run and
  is idempotent (partial material is a hard error, not a regeneration trigger). Both public certs
  are injected via cloud-init, so there is no runtime trust-token exchange.
- **MAC matching**: macOS strips leading zeros in `/var/db/dhcpd_leases` (`52:54:0:...`), so
  `leases::normalize_mac` canonicalises both sides before an exact comparison.
- **Defaults** are `const`s at the top of `main.rs` (bridge CIDR `10.174.0.1/24`, MAC
  `52:54:00:12:34:56`, remote name `malleus`, ports 8443/8444, vsock port 5). The bridge CIDR is
  also hard-coded into the Incus preseed in `provision::render_preseed` — changing one means
  changing both.
- **Name validation**: `valid_mount_name` in `main.rs` gates both mount tags and Incus remote
  names (alphanumeric, `-`, `_`, `.`); `provision::validate_mount_tag` enforces the same rule
  independently on the rendering side.

## Conventions

- TDD. Tests are inline `#[cfg(test)] mod tests` at the bottom of each file; there is no `tests/`
  directory. Write the failing test first and watch it fail for the right reason.
- Test names describe the behaviour and its reason
  (`a_recycled_pid_belonging_to_another_program_is_not_our_vm`), and `expect`/assert messages
  state the expectation rather than restating the call.
- Errors are `thiserror` enums per module, each variant carrying the offending value and the
  remediation in its message (`"...; run \`malleus start\` first"`, `"...; pass \`--vm-ip\`"`).
  Fail loudly and early; `start` reconciles state rather than assuming a clean slate.
- Comments explain *why* (a macOS quirk, a vfkit constraint, a locked decision), not what the
  code does.
- Production-only code paths are guarded with `#[cfg(not(test))]` and their now-unused error
  variants with `#[cfg_attr(test, allow(dead_code))]`.
- Integration tests needing a real Mac are planned as `#[ignore]`, run with `cargo test -- --ignored`.
