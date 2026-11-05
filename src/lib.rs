//! Lease and bound a Linux Cargo build directory shared by multiple checkouts.
//! See the README for the on-disk and command-group contracts.
#[cfg(not(target_os = "linux"))]
compile_error!("cargo-worktree-budget requires Linux");
mod fs;
mod process;
use fs::Identity;
pub use process::{exit_code, forward_signals};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fmt;
use std::fs::{self as disk, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::PathBuf;
use std::process::ExitStatus;
use std::time::Duration;

pub const DEFAULT_BUDGET_BYTES: u64 = 64 << 30;
pub const DEFAULT_RESERVE_BYTES: u64 = 40 << 30;
pub const SESSION_ENV: &str = "CARGO_WORKTREE_BUDGET_SESSION";

/// Command configuration. All paths must be absolute, physical Linux paths.
#[derive(Debug, Clone)]
pub struct Config {
    pub root: PathBuf,
    pub owner: PathBuf,
    pub target_dir: PathBuf,
    pub mount: Option<PathBuf>,
    pub budget_bytes: u64,
    pub reserve_bytes: u64,
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    State(serde_json::Error),
    Policy(String),
    Reserve { available: u64, required: u64 },
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "filesystem or command error: {err}"),
            Self::State(err) => write!(f, "invalid pool state: {err}"),
            Self::Policy(msg) => f.write_str(msg),
            Self::Reserve {
                available,
                required,
            } => write!(
                f,
                "free space {available} bytes below required reserve {required} bytes"
            ),
        }
    }
}
impl std::error::Error for Error {}
impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::State(e)
    }
}
pub type Result<T> = std::result::Result<T, Error>;
pub(crate) fn reject<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::Policy(msg.into()))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Owner {
    path: Vec<u8>,
    directory: Identity,
    target: Vec<u8>,
}
#[derive(Debug, Serialize, Deserialize)]
struct Session {
    token: String,
    boot: String,
    pid_namespace: u64,
    budget: u64,
    reserve: u64,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    format: String,
    root: Identity,
    build: Option<Identity>,
    owner: Option<Owner>,
    running: Option<Session>,
}

const FORMAT: &str = "cargo-worktree-budget/v1";
const STATE: &str = "state.json";
const NEXT: &str = "state.next";
const GROUP: &str = "command.stat";
const BUILD: &str = "build";
fn control(root: &File, name: &str) -> PathBuf {
    fs::link(root).join(name)
}

fn control_file(root: &File, name: &str, write: bool, create: bool) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(write)
        .create_new(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(control(root, name))?;
    fs::check_mount(&file, Some(fs::directory_mount_id(root)?))?;
    let m = file.metadata()?;
    if !m.is_file() || m.nlink() != 1 || m.uid() != root.metadata()?.uid() || m.mode() & 0o077 != 0
    {
        return reject(format!(
            "unexpected control file type, owner, links or permissions: {name}"
        ));
    }
    Ok(file)
}
fn read_control(root: &File, name: &str) -> Result<Vec<u8>> {
    let f = control_file(root, name, false, false)?;
    let mut buf = Vec::new();
    f.take(65537).read_to_end(&mut buf)?;
    if buf.len() > 65536 {
        return reject(format!("oversized pool control file: {name}"));
    }
    Ok(buf)
}
fn missing(err: &Error) -> bool {
    matches!(err,Error::Io(e) if e.kind()==io::ErrorKind::NotFound)
}
fn owner_only_contents(root: &File) -> Result<()> {
    for entry in disk::read_dir(fs::link(root))? {
        let name = entry?.file_name();
        if name == BUILD {
            fs::check_mount(
                &fs::open_dir(&control(root, BUILD))?,
                Some(fs::directory_mount_id(root)?),
            )?;
        } else if let Some(control_name) = [STATE, NEXT, GROUP]
            .into_iter()
            .find(|allowed| name == *allowed)
        {
            control_file(root, control_name, false, false)?;
        } else {
            return reject(format!(
                "unrecognized entry in managed root: {}",
                name.to_string_lossy()
            ));
        }
    }
    Ok(())
}

impl State {
    fn load(root: &File) -> Result<Self> {
        let state: Self = serde_json::from_slice(&read_control(root, STATE)?)?;
        if state.format != FORMAT || state.root != fs::identity(root)? {
            return reject("unrecognized pool format or replaced root directory");
        }
        Ok(state)
    }
    fn save(&self, root: &File) -> Result<()> {
        let buf = serde_json::to_vec(self)?;
        let mut file = match control_file(root, NEXT, true, true) {
            Ok(f) => f,
            Err(Error::Io(e)) if e.kind() == io::ErrorKind::AlreadyExists => {
                control_file(root, NEXT, true, false)?
            }
            Err(e) => return Err(e),
        };
        file.set_len(0)?;
        file.write_all(&buf)?;
        file.sync_all()?;
        disk::rename(control(root, NEXT), control(root, STATE))?;
        root.sync_all()?;
        Ok(())
    }
    fn initialize(root: &File) -> Result<Self> {
        match Self::load(root) {
            Ok(s) => {
                owner_only_contents(root)?;
                return Ok(s);
            }
            Err(e) if missing(&e) => {}
            Err(e) => return Err(e),
        }
        if disk::read_dir(fs::link(root))?.next().is_some() {
            return reject("refusing to adopt a nonempty unregistered pool root");
        }
        let s = Self {
            format: FORMAT.into(),
            root: fs::identity(root)?,
            build: None,
            owner: None,
            running: None,
        };
        s.save(root)?;
        Ok(s)
    }
    fn build(&self, root: &File) -> Result<File> {
        let f = fs::open_dir(&control(root, BUILD))?;
        if self.build.as_ref() != Some(&fs::identity(&f)?) {
            return reject("managed build directory has been replaced");
        }
        fs::check_mount(&f, Some(fs::directory_mount_id(root)?))?;
        Ok(f)
    }
    fn ensure_build(&mut self, root: &File) -> Result<File> {
        if self.build.is_some() {
            return self.build(root);
        }
        match DirBuilder::new().mode(0o700).create(control(root, BUILD)) {
            Ok(()) => root.sync_all()?,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        let f = fs::open_dir(&control(root, BUILD))?;
        fs::check_mount(&f, Some(fs::directory_mount_id(root)?))?;
        fs::private(&f)?;
        if disk::read_dir(fs::link(&f))?.next().is_some() {
            return reject("refusing to adopt a nonempty unregistered build directory");
        }
        self.build = Some(fs::identity(&f)?);
        self.save(root)?;
        Ok(f)
    }
    fn previous_group(&self, root: &File) -> Result<Option<process::Process>> {
        let Some(s) = &self.running else {
            return Ok(None);
        };
        if s.boot != boot()? {
            return Ok(None);
        }
        if s.pid_namespace != disk::metadata("/proc/self/ns/pid")?.ino() {
            return reject(
                "interrupted command belongs to a different PID namespace; recover in the original namespace",
            );
        }
        process::recorded(&read_control(root, GROUP)?)
    }
}

fn boot() -> Result<String> {
    Ok(disk::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .to_owned())
}
fn random_token() -> Result<String> {
    let mut raw = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut raw)?;
    Ok(raw.iter().map(|v| format!("{v:02x}")).collect())
}

/// Read-only inventory. Byte counts are omitted while files may be changing.
#[derive(Debug, Serialize)]
pub struct Status {
    pub initialized: bool,
    pub active: bool,
    pub recovery_required: bool,
    pub owner: Option<String>,
    pub retained_bytes: Option<u64>,
    pub available_bytes: u64,
    pub budget_bytes: u64,
    pub reserve_bytes: u64,
}
impl Config {
    fn validate(&self) -> Result<(Owner, Option<u64>)> {
        for path in [&self.owner, &self.target_dir, &self.root] {
            fs::validate_path(path)?;
        }
        if self.owner.starts_with(&self.root)
            || self.root.starts_with(&self.owner)
            || self.target_dir.starts_with(&self.root)
            || self.root.starts_with(&self.target_dir)
            || self.target_dir == self.owner
            || !self.target_dir.starts_with(&self.owner)
        {
            return reject(
                "pool and checkout must be disjoint and final target must be inside the checkout",
            );
        }
        let mount = self.mount.as_deref().map(fs::require_mount).transpose()?;
        let owner = fs::directory(&self.owner, false)?;
        fs::check_mount(&fs::nearest(&self.root)?, mount)?;
        fs::nearest(&self.target_dir)?;
        Ok((
            Owner {
                path: self.owner.as_os_str().as_bytes().to_owned(),
                directory: fs::identity(&owner)?,
                target: self.target_dir.as_os_str().as_bytes().to_owned(),
            },
            mount,
        ))
    }
    fn root(&self, create: bool, mount: Option<u64>) -> Result<File> {
        let root = fs::directory(&self.root, create)?;
        fs::private(&root)?;
        fs::check_mount(&root, mount)?;
        Ok(root)
    }
    fn available(pool: &File, target: &File) -> Result<u64> {
        let pool_free = fs::free(pool)?;
        if pool.metadata()?.dev() == target.metadata()?.dev() {
            return Ok(pool_free);
        }
        Ok(pool_free.min(fs::free(target)?))
    }
    fn reserve(&self, root: &File, target: &File) -> Result<()> {
        let available = Self::available(root, target)?;
        if available < self.reserve_bytes {
            return Err(Error::Reserve {
                available,
                required: self.reserve_bytes,
            });
        }
        Ok(())
    }
    fn command(&self, args: &[OsString]) -> Result<std::process::Command> {
        let mut cmd = process::command(args)?;
        cmd.env("CARGO_TARGET_DIR", &self.target_dir)
            .env("CARGO_BUILD_BUILD_DIR", self.root.join(BUILD))
            .env_remove("CARGO_BUILD_TARGET_DIR");
        Ok(cmd)
    }
    fn nested(
        &self,
        root: &File,
        owner: &Owner,
        token: &str,
        args: &[OsString],
    ) -> Result<ExitStatus> {
        let state = State::load(root)?;
        let valid = state.owner.as_ref() == Some(owner)
            && state.running.as_ref().is_some_and(|s| {
                s.token == token && self.budget_bytes == s.budget && self.reserve_bytes == s.reserve
            });
        if !valid {
            return reject("invalid nested session, limits, target or checkout owner");
        }
        let Some(group) = state.previous_group(root)? else {
            return reject("nested session has no live command group");
        };
        if !process::is_nested(group)? {
            return reject("nested caller is outside the owning command group");
        }
        state.build(root)?;
        self.reserve(root, &fs::nearest(&self.target_dir)?)?;
        // Nextest and other launchers may have placed the nested wrapper in a
        // child group. Its Cargo command must still belong to the outer watch.
        let mut cmd = self.command(args)?;
        group.join(&mut cmd);
        Ok(cmd.status()?)
    }

    /// Queue for exclusive access, run a command and enforce the retention policy.
    pub fn run(&self, args: &[OsString]) -> Result<ExitStatus> {
        let (owner, mount) = self.validate()?;
        let token = std::env::var_os(SESSION_ENV);
        let root = self.root(token.is_none(), mount)?;
        if let Some(token) = token {
            return self.nested(&root, &owner, token.to_str().unwrap_or(""), args);
        }
        let pid_namespace = disk::metadata("/proc/self/ns/pid")?.ino();
        while !fs::lock(&root, false)? {
            if let Ok(state) = State::load(&root)
                && state
                    .running
                    .as_ref()
                    .is_some_and(|s| s.pid_namespace == pid_namespace)
                && let Some(group) = state.previous_group(&root)?
                && process::is_nested(group)?
            {
                return reject("nested caller lost the owning session environment");
            }
            process::interrupted()?;
            std::thread::sleep(Duration::from_millis(100));
        }
        process::interrupted()?;
        let mut state = State::initialize(&root)?;
        let build = state.ensure_build(&root)?;
        if let Some(group) = state.previous_group(&root)? {
            if process::is_nested(group)? {
                return reject("nested caller lost the owning session environment");
            }
            process::drain(group)?;
        }
        // A queued checkout can have been replaced while another run held the lock.
        if self.validate()?.0 != owner || fs::identity(&self.root(false, mount)?)? != state.root {
            return reject("checkout or pool changed while waiting for the lease");
        }
        let target = fs::nearest(&self.target_dir)?;
        let dirty = state.running.is_some() || state.owner.as_ref() != Some(&owner);
        if dirty
            || fs::tree(&build, false)? > self.budget_bytes
            || Self::available(&root, &target)? < self.reserve_bytes
        {
            // Invalidate the old owner's warm state before the first deletion.
            state.owner = None;
            state.running = None;
            state.save(&root)?;
            fs::tree(&build, true)?;
        }
        self.reserve(&root, &target)?;
        let record = match control_file(&root, GROUP, true, true) {
            Ok(f) => f,
            Err(Error::Io(e)) if e.kind() == io::ErrorKind::AlreadyExists => {
                control_file(&root, GROUP, true, false)?
            }
            Err(e) => return Err(e),
        };
        record.set_len(0)?;
        record.sync_all()?;
        let token = random_token()?;
        state.owner = Some(owner);
        state.running = Some(Session {
            token: token.clone(),
            boot: boot()?,
            pid_namespace,
            budget: self.budget_bytes,
            reserve: self.reserve_bytes,
        });
        state.save(&root)?;
        let mut cmd = self.command(args)?;
        cmd.env(SESSION_ENV, token);
        let child = process::launch(&mut cmd, &record)?;
        let leader = process::recorded(&read_control(&root, GROUP)?)?
            .ok_or_else(|| Error::Policy("missing command group".into()))?;
        let status = process::supervise(child, leader, || self.reserve(&root, &target))
            .and_then(|code| self.reserve(&root, &target).map(|()| code));
        // An I/O error during supervision can leave live children. Preserve the
        // recovery record in that case; the next writer must drain the group.
        if let Err(err) = &status
            && !matches!(err, Error::Reserve { .. })
        {
            return status;
        }
        if fs::tree(&build, false)? > self.budget_bytes || status.is_err() {
            fs::tree(&build, true)?;
        }
        state.running = None;
        state.save(&root)?;
        status
    }
    fn inventory(&self, apply: bool) -> Result<Status> {
        let (_, mount) = self.validate()?;
        let mut status = Status {
            initialized: false,
            active: false,
            recovery_required: false,
            owner: None,
            retained_bytes: Some(0),
            available_bytes: Self::available(
                &fs::nearest(&self.root)?,
                &fs::nearest(&self.target_dir)?,
            )?,
            budget_bytes: self.budget_bytes,
            reserve_bytes: self.reserve_bytes,
        };
        let root = match self.root(false, mount) {
            Err(e) if missing(&e) => return Ok(status),
            other => other?,
        };
        let idle = fs::lock(&root, false)?;
        if !idle {
            if apply {
                return reject("pool is active; prune refused");
            }
            if let Ok(state) = State::load(&root) {
                status.owner = state
                    .owner
                    .map(|o| String::from_utf8_lossy(&o.path).into_owned());
            }
            status.initialized = true;
            status.active = true;
            status.retained_bytes = None;
            return Ok(status);
        }
        let mut state = match State::load(&root) {
            Err(e) if missing(&e) && disk::read_dir(fs::link(&root))?.next().is_none() => {
                return Ok(status);
            }
            other => other?,
        };
        owner_only_contents(&root)?;
        status.initialized = true;
        status.owner = state
            .owner
            .as_ref()
            .map(|o| String::from_utf8_lossy(&o.path).into_owned());
        status.recovery_required = state.running.is_some();
        if let Some(group) = state.previous_group(&root)?
            && process::alive(group)?
        {
            if apply {
                return reject("orphan command group is active; prune refused");
            }
            status.active = true;
            status.retained_bytes = None;
            return Ok(status);
        }
        if state.build.is_some() {
            let build = state.build(&root)?;
            if apply {
                state.owner = None;
                state.running = None;
                state.save(&root)?;
                fs::tree(&build, true)?;
                status.owner = None;
                status.recovery_required = false;
            }
            status.retained_bytes = Some(fs::tree(&build, false)?);
        }
        status.available_bytes = Self::available(&root, &fs::nearest(&self.target_dir)?)?;
        Ok(status)
    }
    /// Inspect without creating directories or files. Busy pools are not scanned.
    pub fn status(&self) -> Result<Status> {
        self.inventory(false)
    }
    /// Drop all registered intermediates only when apply is true and the pool is idle.
    pub fn prune(&self, apply: bool) -> Result<Status> {
        self.inventory(apply)
    }
}
