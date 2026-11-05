//! Start a process group with a crash-recovery record before exec.
use crate::{Error, Result, reject};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug)]
pub(crate) struct Process {
    pid: i32,
    parent: i32,
    group: i32,
    start: u64,
    zombie: bool,
}

fn parse(raw: &str) -> Option<Process> {
    let (id, fields) = raw.rsplit_once(')')?;
    let pid = id.split_once(' ')?.0.parse().ok()?;
    let mut fields = fields.split_whitespace();
    let state = fields.next()?;
    let parent = fields.next()?.parse().ok()?;
    let group = fields.next()?.parse().ok()?;
    let start = fields.nth(16)?.parse().ok()?;
    Some(Process {
        pid,
        parent,
        group,
        start,
        zombie: state == "Z" || state == "X",
    })
}
fn process(pid: i32) -> Result<Option<Process>> {
    match fs::read(format!("/proc/{pid}/stat")) {
        Ok(bytes) => parse(&String::from_utf8_lossy(&bytes))
            .map(Some)
            .ok_or_else(|| Error::Policy("invalid Linux process stat".into())),
        Err(err) if matches!(err.raw_os_error(), Some(libc::ENOENT | libc::ESRCH)) => Ok(None),
        Err(err) => Err(err.into()),
    }
}
pub(crate) fn recorded(record: &[u8]) -> Result<Option<Process>> {
    if record.is_empty() {
        return Ok(None);
    }
    let p = parse(&String::from_utf8_lossy(record))
        .ok_or_else(|| Error::Policy("invalid command group record".into()))?;
    if p.pid < 2 || p.group != p.pid {
        return reject("invalid command group leader");
    }
    Ok(Some(p))
}
pub(crate) fn is_nested(leader: Process) -> Result<bool> {
    if let Some(live) = process(leader.pid)?
        && live.start != leader.start
    {
        return Ok(false);
    }
    let mut pid = std::process::id() as i32;
    while pid > 1 {
        let Some(p) = process(pid)? else {
            return Ok(false);
        };
        if p.group == leader.pid || (p.pid == leader.pid && p.start == leader.start) {
            return Ok(true);
        }
        pid = p.parent;
    }
    Ok(false)
}

pub(crate) fn alive(leader: Process) -> Result<bool> {
    if let Some(p) = process(leader.pid)? {
        if p.start != leader.start {
            return Ok(false);
        }
        if !p.zombie && p.group == leader.pid {
            return Ok(true);
        }
    }
    // If the leader has exited, grandchildren may still be compiling.
    // SAFETY: signal 0 only queries an exact process group.
    if unsafe { libc::kill(-leader.pid, 0) } != 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            return Ok(false);
        }
        return Err(err.into());
    }
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse().ok()) else {
            continue;
        };
        if let Some(p) = process(pid)?
            && p.group == leader.pid
            && !p.zombie
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn drain(leader: Process) -> Result<()> {
    while alive(leader)? {
        interrupted()?;
        std::thread::sleep(POLL);
    }
    Ok(())
}

pub(crate) fn launch(cmd: &mut Command, record: &File) -> Result<Child> {
    let fd = record.as_raw_fd();
    cmd.process_group(0);
    // SAFETY: after fork use only async-signal-safe syscalls and stack storage.
    // The parent's flock remains open in this child until exec. In particular,
    // a killed parent cannot expose an executable writer without its group id.
    unsafe {
        cmd.pre_exec(move || {
            let procfd = libc::open(
                c"/proc/self/stat".as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC,
            );
            if procfd < 0 {
                return Err(io::Error::last_os_error());
            }
            let mut buf = [0u8; 2048];
            let n = libc::read(procfd, buf.as_mut_ptr().cast(), buf.len());
            libc::close(procfd);
            if n < 1 {
                return Err(io::Error::last_os_error());
            }
            if libc::pwrite(fd, buf.as_ptr().cast(), n as usize, 0) != n || libc::fsync(fd) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn().map_err(Error::Io)
}

static SIGNAL: AtomicI32 = AtomicI32::new(0);
extern "C" fn received(signal: i32) {
    SIGNAL.store(signal, Ordering::Relaxed);
}
/// Forward termination signals to the managed command group. Call once from a CLI.
pub fn forward_signals() -> Result<()> {
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
        // SAFETY: the handler only stores into a lock-free atomic integer.
        if unsafe { libc::signal(sig, received as *const () as libc::sighandler_t) }
            == libc::SIG_ERR
        {
            return Err(io::Error::last_os_error().into());
        }
    }
    Ok(())
}
pub(crate) fn interrupted() -> Result<()> {
    let signal = SIGNAL.load(Ordering::Relaxed);
    if signal != 0 {
        return reject(format!("interrupted by signal {signal}"));
    }
    Ok(())
}

fn signal_group(leader: Process, sig: i32) -> Result<()> {
    if !alive(leader)? {
        return Ok(());
    }
    // SAFETY: group belongs to the recorded command; never signals the caller's group.
    if unsafe { libc::kill(-leader.pid, sig) } != 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ESRCH) {
            return Err(err.into());
        }
    }
    Ok(())
}

pub(crate) fn supervise(
    mut child: Child,
    leader: Process,
    mut check: impl FnMut() -> Result<()>,
) -> Result<ExitStatus> {
    let mut failure = None;
    let mut stopping = None;
    let mut status = None;
    loop {
        if status.is_none() {
            status = child.try_wait()?;
        }
        if status.is_some() && !alive(leader)? {
            break;
        }
        let requested = SIGNAL.swap(0, Ordering::Relaxed);
        if stopping.is_none() {
            failure = check().err();
            if failure.is_some() || requested != 0 {
                signal_group(
                    leader,
                    if requested != 0 {
                        requested
                    } else {
                        libc::SIGTERM
                    },
                )?;
                stopping = Some(Instant::now());
            }
        } else if stopping.is_some_and(|t| t.elapsed() >= Duration::from_secs(1)) {
            signal_group(leader, libc::SIGKILL)?;
        }
        std::thread::sleep(POLL);
    }
    if let Some(err) = failure {
        return Err(err);
    }
    status.ok_or_else(|| Error::Policy("command produced no exit status".into()))
}
pub(crate) fn command(args: &[OsString]) -> Result<Command> {
    let (name, args) = args
        .split_first()
        .ok_or_else(|| Error::Policy("run requires a command".into()))?;
    let mut cmd = Command::new(name);
    cmd.args(args);
    Ok(cmd)
}
pub fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
}
