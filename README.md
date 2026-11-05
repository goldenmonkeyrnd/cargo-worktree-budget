# cargo-worktree-budget

Build several checkouts of a Rust project while retaining one warm set of Cargo intermediates. Each checkout keeps its own final binaries. Concurrent top-level commands queue for the same project slot. The first command from a different checkout clears the intermediates before starting Cargo, including fingerprint files and build-script outputs.

## Run a build

Build the manager on Linux with `cargo build --release --locked`, or install the checkout with `cargo install --path . --locked`. Use Cargo 1.91 or newer for the managed build. The pool filesystem needs Linux directory locking, creation times, mount IDs and directory fsync. Use a local, single-user filesystem and a mounted `/proc`.

From a project checkout, choose a pool outside all its worktrees. Use the same pool for every checkout of that project.

```sh
checkout=$(pwd -P)
pool="${XDG_CACHE_HOME:-$HOME/.cache}/cargo-worktree-budget/example"
cargo-worktree-budget --root "$pool" --owner "$checkout" \
  --target-dir "$checkout/target" run -- cargo build --locked
```

The manager sets `CARGO_BUILD_BUILD_DIR` to `$pool/slot/build` and `CARGO_TARGET_DIR` to the requested checkout target. The command inherits its arguments, working directory and standard streams. Its exit code reaches the caller. Keep build scripts and Cargo configuration consistent with these paths. Apply overrides such as coverage-specific child directories only inside the corresponding managed directories.

| Global option | Meaning |
| --- | --- |
| `--root PATH` | Dedicated pool directory. On first use it must be absent or empty. Existing roots must have mode 0700 and belong to the current user. |
| `--owner CHECKOUT` | Existing physical checkout directory. Ownership includes the path, device, inode and creation time. |
| `--target-dir DIR` | Final target below the checkout. The manager never clears this directory. |
| `--mount PATH` | Require this mounted filesystem to contain the pool. A missing mount fails closed. The checkout can live on another filesystem. |
| `--budget-bytes N` | Retain at most this many allocated bytes of intermediates between commands. Default 64 GiB (68719476736 bytes). |
| `--reserve-bytes N` | Minimum available bytes on the pool and final-target filesystems. Default 40 GiB (42949672960 bytes). |

The manager refuses symlinks in configured paths. Give it exclusive use of the pool and keep its parent directories under trusted control. The manager records the identity of a private directory at `$pool/slot`. Cargo can remove and recreate its `build` child during `cargo clean`. Everything inside that child is disposable. Before counting or deleting its contents, the manager opens the current child and verifies its mount. It unlinks symlinks inside the managed tree without following them. An unexpected file at the top level, a replaced slot directory, or unreadable state requires operator inspection.

## Storage and crash recovery

Before a run, the manager checks the free-space reserve and the retained size. If necessary, it clears the pool and checks free space again. During a run it checks free space every 250 ms. A breach terminates the command process group, allows one second for shutdown, then sends SIGKILL to remaining group members. It clears intermediates after the group exits. Writes between samples can temporarily cross the reserve. The retention budget is checked at command boundaries and does not cap peak build size.

A kernel directory lock protects changes to the pool. Before exec, the child durably records its Linux process group. The manager waits for members of that group, including grandchildren, before relinquishing ownership. If the manager is killed, a subsequent run waits for survivors and discards the interrupted build before reuse. Monitor and recover a pool from the same PID namespace. For command supervision, keep build processes in the launched process group. A program that deliberately detaches from supervision must not write to the managed tree. A supervisor killed by SIGKILL cannot continue sampling disk space.

For systemd scopes, start the manager **inside** the scope and launch Cargo beneath it. Recursive wrappers can call the manager again with the original configuration. Re-entry requires both the live session token and verified Linux process ancestry or group membership for that checkout. Queued callers cannot reuse a token copied from a different process. Run metadata-only queries through the host wrapper's read-only path if they should leave the build slot untouched. Explicit `run` always takes the build slot.

## Inspect or reclaim

```sh
cargo-worktree-budget --root "$pool" --owner "$checkout" --target-dir "$checkout/target" status
cargo-worktree-budget --root "$pool" --owner "$checkout" --target-dir "$checkout/target" prune
cargo-worktree-budget --root "$pool" --owner "$checkout" --target-dir "$checkout/target" prune --apply
```

`status` and the default `prune` print JSON without creating files or directories. `retained_bytes` is the allocated size of entries inside the build directory, counting each inode once. It is null during active use. `available_bytes` is the lower free-space value across the pool and target filesystems. `prune --apply` clears the idle slot’s build directory and forgets its owner. An active pool refuses the request.

Rust callers can construct `cargo_worktree_budget::Config` and call `run`, `status` or `prune`. The CLI installs termination-signal forwarding via `forward_signals`; embedding applications choose their own process-wide signal policy.

## Verify

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

The integration tests launch real shell and Cargo processes. The disk-reserve test temporarily writes 64 MiB in the system temporary directory.
