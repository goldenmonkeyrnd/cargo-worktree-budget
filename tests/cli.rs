use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(0);
const BIN: &str = env!("CARGO_BIN_EXE_cargo-worktree-budget");
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "cargo-worktree-budget-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        for owner in ["one", "two"] {
            fs::create_dir(path.join(owner)).unwrap();
        }
        Self(path)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
    fn manager(&self, owner: &str) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.arg("--root")
            .arg(self.path("pool"))
            .arg("--owner")
            .arg(self.path(owner))
            .arg("--target-dir")
            .arg(self.path(owner).join("target"))
            .args(["--reserve-bytes", "0"]);
        cmd
    }
    fn run(&self, owner: &str, script: &str) -> Output {
        self.manager(owner)
            .args(["run", "--", "sh", "-ec", script])
            .output()
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn ok(out: Output) {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
#[test]
fn ownership_and_checkout_recreation() {
    let f = Fixture::new();
    for action in ["status", "prune"] {
        ok(f.manager("one").arg(action).output().unwrap());
        assert!(!f.path("pool").exists());
    }
    ok(f.run("one",r#"echo cache > "$CARGO_BUILD_BUILD_DIR/flag"; mkdir -p "$CARGO_TARGET_DIR"; echo binary > "$CARGO_TARGET_DIR/final""#));
    ok(f.run("one", r#"test -f "$CARGO_BUILD_BUILD_DIR/flag""#));
    ok(f.run(
        "two",
        r#"test ! -e "$CARGO_BUILD_BUILD_DIR/flag"; touch "$CARGO_BUILD_BUILD_DIR/second""#,
    ));
    assert!(f.path("one/target/final").exists());
    fs::rename(f.path("two"), f.path("old")).unwrap();
    fs::create_dir(f.path("two")).unwrap();
    ok(f.run("two", r#"test ! -e "$CARGO_BUILD_BUILD_DIR/second""#));
}
#[test]
fn refuse_foreign_root_and_symlinks() {
    let f = Fixture::new();
    fs::create_dir(f.path("pool")).unwrap();
    fs::set_permissions(f.path("pool"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(f.path("pool/valuable"), "keep").unwrap();
    assert!(!f.run("one", "true").status.success());
    assert_eq!(fs::read(f.path("pool/valuable")).unwrap(), b"keep");
    fs::rename(f.path("pool"), f.path("foreign")).unwrap();
    symlink(f.path("foreign"), f.path("pool")).unwrap();
    assert!(!f.run("one", "true").status.success());
    assert!(f.path("foreign/valuable").exists());
}
#[test]
fn soft_budget_and_reserve() {
    let f = Fixture::new();
    let out = f
        .manager("one")
        .args([
            "--budget-bytes",
            "1",
            "run",
            "--",
            "sh",
            "-ec",
            r#"dd if=/dev/zero of="$CARGO_BUILD_BUILD_DIR/blob" bs=4K count=4 2>/dev/null"#,
        ])
        .output()
        .unwrap();
    ok(out);
    assert!(!f.path("pool/build/blob").exists());
    let mut c = Command::new(BIN);
    let out = c
        .arg("--root")
        .arg(f.path("pool"))
        .arg("--owner")
        .arg(f.path("one"))
        .arg("--target-dir")
        .arg(f.path("one/target"))
        .args([
            "--reserve-bytes",
            "18446744073709551615",
            "run",
            "--",
            "touch",
        ])
        .arg(f.path("should-never-run"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!f.path("should-never-run").exists());
}
#[test]
fn symlink_inside_build_is_never_followed() {
    let f = Fixture::new();
    fs::write(f.path("one/valuable"), "keep").unwrap();
    ok(f.run("one", "true"));
    symlink(f.path("one"), f.path("pool/build/link")).unwrap();
    ok(f.run("two", "true"));
    assert!(f.path("one/valuable").exists());
    assert!(!f.path("pool/build/link").exists());
}
fn cargo(f: &Fixture, owner: &str, verb: &str) -> Output {
    f.manager(owner)
        .args(["run", "--"])
        .arg(std::env::var_os("CARGO").unwrap())
        .args([verb, "--offline"])
        .current_dir(f.path(owner))
        .output()
        .unwrap()
}

fn make_older(file: &Path) {
    ok(Command::new("touch")
        .args(["-t", "200001010000"])
        .arg(file)
        .output()
        .unwrap());
}

fn expect_witness(f: &Fixture, owner: &str, value: &str) {
    let binary = f.path(owner).join("target/debug/witness");
    assert_eq!(
        Command::new(binary).output().unwrap().stdout,
        format!("{value}\n").as_bytes()
    );
    assert!(!f.path(owner).join("target/debug/.fingerprint").exists());
    let fingerprints = f.path("pool/build/debug/.fingerprint");
    assert!(fs::read_dir(fingerprints).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("witness-")
    }));
}

#[test]
fn cargo_rebuilds_divergent_older_sources() {
    let f = Fixture::new();
    for (owner, value) in [("one", "AAAA"), ("two", "BBBB")] {
        let ws = f.path(owner);
        fs::create_dir(ws.join("src")).unwrap();
        fs::write(
            ws.join("Cargo.toml"),
            "[package]\nname='witness'\nversion='0.1.0'\nedition='2021'\n",
        )
        .unwrap();
        fs::write(ws.join("src/main.rs"),format!(r#"fn main() {{ println!("{value}"); }}
#[test] fn checkout_behavior() {{ assert_eq!("{value}", "AAAA", "branch test must fail for BBBB"); }}"#)).unwrap();
        if owner == "two" {
            make_older(&ws.join("src/main.rs"));
        }
    }
    ok(cargo(&f, "one", "build"));
    ok(cargo(&f, "one", "test"));
    expect_witness(&f, "one", "AAAA");
    ok(cargo(&f, "two", "build"));
    expect_witness(&f, "two", "BBBB");
    let failed = cargo(&f, "two", "test");
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stdout).contains("branch test must fail for BBBB"));
    ok(cargo(&f, "one", "build"));
    ok(cargo(&f, "one", "test"));
    expect_witness(&f, "one", "AAAA");
    let bad = f.path("two/src/main.rs");
    fs::write(
        &bad,
        "compile_error!(\"invalid older checkout source\"); fn main() {}",
    )
    .unwrap();
    make_older(&bad);
    let invalid = cargo(&f, "two", "build");
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("invalid older checkout source"));
    expect_witness(&f, "one", "AAAA");
}

fn wait_file(path: &Path) {
    for _ in 0..250 {
        if path.exists() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("timed out waiting for {}", path.display());
}
#[test]
fn competing_runs_wait_even_after_manager_sigkill() {
    let f = Fixture::new();
    let mut first=f.manager("one").current_dir(&f.0).args(["run","--","sh","-ec",r#"touch ready "$CARGO_BUILD_BUILD_DIR/dirty"; while [ ! -e release ]; do sleep 0.03; done; echo old > "$CARGO_BUILD_BUILD_DIR/last""#]).spawn().unwrap();
    wait_file(&f.path("ready"));
    let report = f.manager("two").arg("status").output().unwrap();
    assert!(
        serde_json::from_slice::<serde_json::Value>(&report.stdout).unwrap()["active"]
            .as_bool()
            .unwrap()
    );
    assert!(
        !f.manager("two")
            .args(["prune", "--apply"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let mut queued=f.manager("two").current_dir(&f.0).args(["run","--","sh","-ec",r#"test ! -e "$CARGO_BUILD_BUILD_DIR/dirty"; test ! -e "$CARGO_BUILD_BUILD_DIR/last"; touch new-owner"#]).spawn().unwrap();
    first.kill().unwrap();
    first.wait().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(350));
    assert!(queued.try_wait().unwrap().is_none());
    assert!(!f.path("new-owner").exists());
    fs::write(f.path("release"), "").unwrap();
    assert!(queued.wait().unwrap().success());
    assert!(f.path("new-owner").exists());
}
#[test]
fn authentic_nested_commands_and_forged_tokens() {
    let f = Fixture::new();
    let mut cmd = f.manager("one");
    cmd.env("BIN", BIN).env("BASE", &f.0);
    ok(cmd.args(["run","--","sh","-ec",r#"
 "$BIN" --root "$BASE/pool" --owner "$BASE/one" --target-dir "$BASE/one/target" --reserve-bytes 0 run -- touch "$BASE/nested"
 ! "$BIN" --root "$BASE/pool" --owner "$BASE/two" --target-dir "$BASE/two/target" --reserve-bytes 0 run -- touch "$BASE/wrong-owner"
 "#]).output().unwrap());
    assert!(f.path("nested").exists());
    assert!(!f.path("wrong-owner").exists());
    assert!(
        !f.manager("one")
            .env("CARGO_WORKTREE_BUDGET_SESSION", "invented")
            .args(["run", "--", "false"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let mut holder=f.manager("one").current_dir(&f.0).args(["run","--","sh","-ec",r#"echo "$CARGO_WORKTREE_BUDGET_SESSION" > partial; mv partial token; while [ ! -e release ]; do sleep 0.03; done"#]).spawn().unwrap();
    wait_file(&f.path("token"));
    let token = fs::read_to_string(f.path("token")).unwrap();
    let attempt = f
        .manager("one")
        .env("CARGO_WORKTREE_BUDGET_SESSION", token.trim())
        .args(["run", "--", "touch"])
        .arg(f.path("forged"))
        .output()
        .unwrap();
    fs::write(f.path("release"), "").unwrap();
    assert!(holder.wait().unwrap().success());
    assert!(!attempt.status.success());
    assert!(!f.path("forged").exists());
}
#[test]
fn preserve_replaced_build_directory_and_reject_unmounted_path() {
    let f = Fixture::new();
    assert!(
        !f.manager("one")
            .arg("--mount")
            .arg(&f.0)
            .args(["run", "--", "true"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(!f.path("pool").exists());
    ok(f.run("one", "true"));
    fs::rename(f.path("pool/build"), f.path("displaced")).unwrap();
    fs::create_dir(f.path("pool/build")).unwrap();
    fs::write(f.path("pool/build/foreign"), "save").unwrap();
    assert!(
        !f.manager("one")
            .args(["prune", "--apply"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(f.path("pool/build/foreign").exists());
}
#[test]
fn live_reserve_terminates_owned_grandchildren_only() {
    let f = Fixture::new();
    ok(f.run("one", "true"));
    let report = f.manager("one").arg("status").output().unwrap();
    let status: serde_json::Value = serde_json::from_slice(&report.stdout).unwrap();
    let reserve = status["available_bytes"]
        .as_u64()
        .unwrap()
        .saturating_sub(32 << 20);
    let mut cmd = Command::new(BIN);
    let mut managed = cmd
        .current_dir(&f.0)
        .arg("--root")
        .arg(f.path("pool"))
        .arg("--owner")
        .arg(f.path("one"))
        .arg("--target-dir")
        .arg(f.path("one/target"))
        .arg("--reserve-bytes")
        .arg(reserve.to_string())
        .args([
            "run",
            "--",
            "sh",
            "-ec",
            r#"
 trap '' TERM; (trap '' TERM; touch started; sleep 4; touch escaped) &
 dd if=/dev/zero of="$CARGO_BUILD_BUILD_DIR/blob" bs=1M count=64 status=none
 wait
 "#,
        ])
        .spawn()
        .unwrap();
    let mut independent = Command::new("sleep").arg("10").spawn().unwrap();
    let result = managed.wait().unwrap();
    assert!(independent.try_wait().unwrap().is_none());
    independent.kill().unwrap();
    independent.wait().unwrap();
    assert!(f.path("started").exists());
    assert!(!result.success());
    assert!(!f.path("pool/build/blob").exists());
    assert!(!f.path("escaped").exists());
}

#[test]
fn pool_mount_does_not_relocate_checkout_outputs() {
    let f = Fixture::new();
    // This fixture reads the separate tmpfs without creating a target there.
    let checkout = Path::new("/dev/shm");
    if !checkout.is_dir() {
        return;
    }
    let mount = Command::new("stat")
        .args(["-c", "%m"])
        .arg(&f.0)
        .output()
        .unwrap();
    assert!(mount.status.success());
    let mount = String::from_utf8(mount.stdout).unwrap();
    let target = checkout.join(format!("cargo-budget-unused-{}", std::process::id()));
    let mut cmd = Command::new(BIN);
    cmd.arg("--root")
        .arg(f.path("pool"))
        .arg("--owner")
        .arg(checkout)
        .arg("--target-dir")
        .arg(&target)
        .arg("--mount")
        .arg(mount.trim())
        .args(["--reserve-bytes", "0", "run", "--", "true"]);
    ok(cmd.output().unwrap());
    assert!(!target.exists());
}

#[test]
fn bind_mounted_control_files_are_never_modified() {
    if !Command::new("unshare")
        .args(["-rm", "true"])
        .status()
        .is_ok_and(|s| s.success())
    {
        eprintln!("mount namespace unavailable; skipping bind-mount witness");
        return;
    }
    let f = Fixture::new();
    ok(f.run("one", "true"));
    fs::write(f.path("sentinel"), "valuable external contents").unwrap();
    fs::set_permissions(f.path("sentinel"), fs::Permissions::from_mode(0o600)).unwrap();
    let witness = r#"
        manager() {
            "$BIN" --root "$BASE/pool" --owner "$BASE/one" \
                --target-dir "$BASE/one/target" --reserve-bytes 0 "$@"
        }
        for control in command.stat state.json state.next; do
            if [ "$control" = state.next ]; then install -m 600 /dev/null "$BASE/pool/$control"; fi
            mount --bind "$BASE/sentinel" "$BASE/pool/$control"
            ! manager run -- true || exit 11
            ! manager status || exit 12
            ! manager prune --apply || exit 13
            test "$(cat "$BASE/sentinel")" = 'valuable external contents' || exit 14
            umount "$BASE/pool/$control"
        done
    "#;
    ok(Command::new("unshare")
        .env("BIN", BIN)
        .env("BASE", &f.0)
        .args(["-rm", "sh", "-ec", witness])
        .output()
        .unwrap());
}

#[test]
fn orphan_reentry_without_token_refuses_to_drain_itself() {
    let f = Fixture::new();
    let script = r#"
        touch ready
        while [ ! -e killed ]; do sleep 0.02; done
        result=0
        timeout 2 env -u CARGO_WORKTREE_BUDGET_SESSION \
            "$BIN" --root "$BASE/pool" --owner "$BASE/one" --target-dir "$BASE/one/target" \
            --reserve-bytes 0 run -- touch should-not-run 2>diagnostic || result=$?
        echo "$result" >result
    "#;
    let mut first = f
        .manager("one")
        .env("BIN", BIN)
        .env("BASE", &f.0)
        .current_dir(&f.0)
        .args(["run", "--", "sh", "-ec", script])
        .spawn()
        .unwrap();
    wait_file(&f.path("ready"));
    first.kill().unwrap();
    first.wait().unwrap();
    fs::write(f.path("killed"), "").unwrap();
    wait_file(&f.path("result"));
    let result = fs::read_to_string(f.path("result")).unwrap();
    assert_eq!(
        result.trim(),
        "1",
        "{}",
        fs::read_to_string(f.path("diagnostic")).unwrap()
    );
    assert!(
        fs::read_to_string(f.path("diagnostic"))
            .unwrap()
            .contains("lost the owning session")
    );
    assert!(!f.path("should-not-run").exists());
    ok(f.run("two", "true"));
}
