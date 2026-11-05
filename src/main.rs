use cargo_worktree_budget::{Config, DEFAULT_BUDGET_BYTES, DEFAULT_RESERVE_BYTES, Error, Result};
use std::ffi::OsString;
use std::path::PathBuf;

const HELP: &str = "cargo-worktree-budget [OPTIONS] run -- COMMAND [ARGS...]\n\
cargo-worktree-budget [OPTIONS] status\n\
cargo-worktree-budget [OPTIONS] prune [--apply]\n\n\
Required: --root PATH --owner CHECKOUT --target-dir PATH\n\
Optional: --mount PATH --budget-bytes N (default 64 GiB) --reserve-bytes N (default 40 GiB)\n\
Use absolute paths. status and prune without --apply never change files.";
fn invalid(msg: impl Into<String>) -> Error {
    Error::Policy(msg.into())
}
fn bytes(value: &OsString) -> Result<u64> {
    value
        .to_str()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| invalid("expected an unsigned byte count"))
}
fn cli() -> Result<i32> {
    let mut args = std::env::args_os().skip(1).peekable();
    let (mut root, mut owner, mut target_dir, mut mount) = (None, None, None, None);
    let (mut budget_bytes, mut reserve_bytes) = (DEFAULT_BUDGET_BYTES, DEFAULT_RESERVE_BYTES);
    let action = loop {
        let arg = args.next().ok_or_else(|| invalid(HELP))?;
        match arg.to_str() {
            Some("--help" | "-h") => {
                println!("{HELP}");
                return Ok(0);
            }
            Some("--version" | "-V") => {
                println!("cargo-worktree-budget {}", env!("CARGO_PKG_VERSION"));
                return Ok(0);
            }
            Some("run" | "status" | "prune") => break arg,
            Some(
                "--root" | "--owner" | "--target-dir" | "--mount" | "--budget-bytes"
                | "--reserve-bytes",
            ) => {
                let val = args
                    .next()
                    .ok_or_else(|| invalid(format!("{} needs a value", arg.display())))?;
                match arg.to_str() {
                    Some("--root") => root = Some(PathBuf::from(val)),
                    Some("--owner") => owner = Some(PathBuf::from(val)),
                    Some("--target-dir") => target_dir = Some(PathBuf::from(val)),
                    Some("--mount") => mount = Some(PathBuf::from(val)),
                    Some("--budget-bytes") => budget_bytes = bytes(&val)?,
                    _ => reserve_bytes = bytes(&val)?,
                }
            }
            _ => {
                return Err(invalid(format!(
                    "unexpected argument: {}\n{HELP}",
                    arg.display()
                )));
            }
        }
    };
    let cfg = Config {
        root: root.ok_or_else(|| invalid("missing --root"))?,
        owner: owner.ok_or_else(|| invalid("missing --owner"))?,
        target_dir: target_dir.ok_or_else(|| invalid("missing --target-dir"))?,
        mount,
        budget_bytes,
        reserve_bytes,
    };
    match action.to_str() {
        Some("run") => {
            if args.next().as_deref() != Some(std::ffi::OsStr::new("--")) || args.peek().is_none() {
                return Err(invalid("expected run -- COMMAND [ARGS...]"));
            }
            cargo_worktree_budget::forward_signals()?;
            Ok(cargo_worktree_budget::exit_code(
                cfg.run(&args.collect::<Vec<_>>())?,
            ))
        }
        _ => {
            let apply = action == "prune" && args.next_if(|a| a == "--apply").is_some();
            if args.next().is_some() {
                return Err(invalid("unexpected status/prune argument"));
            }
            println!("{}", serde_json::to_string_pretty(&cfg.prune(apply)?)?);
            Ok(0)
        }
    }
}
fn main() {
    match cli() {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("cargo-worktree-budget: {err}");
            std::process::exit(1);
        }
    }
}
