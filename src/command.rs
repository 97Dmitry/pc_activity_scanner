//! Bounded unprivileged CLI calls for sampling workers, never GUI callbacks.

use std::ffi::OsStr;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

const MAX_STDOUT_BYTES: usize = 256 * 1024;
const MAX_ACTIVE_READERS: usize = 16;
const SUPERVISOR_POLL: Duration = Duration::from_millis(25);
static ACTIVE_READERS: AtomicUsize = AtomicUsize::new(0);

struct ReaderPermit<'a>(&'a AtomicUsize);
impl<'a> ReaderPermit<'a> {
    fn acquire(counter: &'a AtomicUsize) -> Option<Self> {
        counter
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                if count < MAX_ACTIVE_READERS {
                    Some(count + 1)
                } else {
                    None
                }
            })
            .ok()
            .map(|_| Self(counter))
    }
}
impl Drop for ReaderPermit<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

struct OwnedChild {
    child: Child,
    reaped: bool,
    #[cfg(target_os = "windows")]
    job: KillJob,
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !self.reaped {
            // This handle was created by this invocation. Never search by name
            // or signal arbitrary PIDs or process groups. The Windows job owns
            // only this command and descendants created inside its private job.
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(target_os = "windows")]
struct KillJob(std::os::windows::io::OwnedHandle);
#[cfg(target_os = "windows")]
impl KillJob {
    fn new() -> Result<Self, String> {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };
        // Unnamed, non-inheritable, and only owned by this command invocation.
        // Never open or alter the GUI's inherited job or any host job.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(format!(
                "Command job create failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let job = Self(unsafe { OwnedHandle::from_raw_handle(handle) });
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if unsafe {
            SetInformationJobObject(
                job.0.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(format!(
                "Command job limits failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(job)
    }
    fn assign(&self, child: &Child) -> Result<(), String> {
        use std::os::windows::io::AsRawHandle;
        if unsafe {
            windows_sys::Win32::System::JobObjects::AssignProcessToJobObject(
                self.0.as_raw_handle(),
                child.as_raw_handle(),
            )
        } == 0
        {
            return Err(format!(
                "Command job assignment failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }
}

fn spawn_owned(command: &mut Command) -> Result<OwnedChild, String> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        unsafe extern "C" {
            fn getpid() -> i32;
            fn getppid() -> i32;
            fn prctl(option: i32, ...) -> i32;
        }
        let parent = unsafe { getpid() };
        // Only async-signal-safe syscalls run between fork and exec. The parent
        // check closes the race where the creator died before installing SIGKILL.
        unsafe {
            command.pre_exec(move || {
                if prctl(1, 9_usize, 0_usize, 0_usize, 0_usize) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if getppid() != parent {
                    return Err(std::io::Error::from_raw_os_error(3));
                }
                Ok(())
            });
        }
    }
    #[cfg(target_os = "windows")]
    let job = KillJob::new()?;
    let child = OwnedChild {
        child: command
            .spawn()
            .map_err(|error| format!("Command spawn failed: {error}"))?,
        reaped: false,
        #[cfg(target_os = "windows")]
        job,
    };
    #[cfg(target_os = "windows")]
    child.job.assign(&child.child)?;
    Ok(child)
}

fn read_stdout(mut output: impl Read) -> Result<String, String> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        match output.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                if bytes.len().saturating_add(count) > MAX_STDOUT_BYTES {
                    return Err("Command stdout limit exceeded (256 KiB)".into());
                }
                bytes.extend_from_slice(&chunk[..count]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("Command stdout read failed: {error}")),
        }
    }
    String::from_utf8(bytes).map_err(|_| "Command stdout is not UTF-8".into())
}

/// Run a fixed, unprivileged source command on a background sampling worker.
/// No shell is inserted; callers own the allowlist of literal program/arguments.
/// A grandchild retaining stdout cannot hold this call beyond its deadline. Its
/// reader can live until EOF, but no more than 16 reader threads can exist.
pub fn run(
    program: &OsStr,
    args: &[&str],
    stop: &AtomicBool,
    timeout: Duration,
) -> Result<String, String> {
    run_with_cancel(program, args, stop, timeout, || false)
}

/// Additional cancellation for a caller's generation or operation scope.
/// The predicate is checked before/after spawn and at each supervisor poll.
/// It cannot recall work the child has already submitted to another OS service.
pub fn run_with_cancel(
    program: &OsStr,
    args: &[&str],
    stop: &AtomicBool,
    timeout: Duration,
    cancelled: impl Fn() -> bool,
) -> Result<String, String> {
    let started = Instant::now();
    if stop.load(Ordering::Relaxed) || cancelled() {
        return Err("Command cancelled".into());
    }
    if timeout.is_zero() {
        return Err("Command timed out".into());
    }
    let permit =
        ReaderPermit::acquire(&ACTIVE_READERS).ok_or("Command stdout reader capacity exhausted")?;
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    let mut child = spawn_owned(&mut command)?;
    if stop.load(Ordering::Relaxed) || cancelled() {
        return Err("Command cancelled".into());
    }
    let stdout = child
        .child
        .stdout
        .take()
        .ok_or("Command stdout pipe is missing")?;
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("resource-command-output".into())
        .spawn(move || {
            let _permit = permit;
            let _ = sender.send(read_stdout(stdout));
        })
        .map_err(|error| format!("Command stdout reader failed to start: {error}"))?;
    let mut output = None;
    let mut failed_status = None;
    loop {
        if stop.load(Ordering::Relaxed) || cancelled() {
            return Err("Command cancelled".into());
        }
        if started.elapsed() >= timeout {
            return Err("Command timed out".into());
        }
        if !child.reaped
            && let Some(status) = child
                .child
                .try_wait()
                .map_err(|error| format!("Command status failed: {error}"))?
        {
            child.reaped = true;
            if !status.success() {
                failed_status = Some(format!("Command exited with status {status}"));
            }
        }
        if child.reaped
            && let Some(output) = output.take()
        {
            return failed_status.map_or_else(|| Ok(output), Err);
        }
        let wait = SUPERVISOR_POLL.min(timeout.saturating_sub(started.elapsed()));
        if output.is_none() {
            match receiver.recv_timeout(wait) {
                Ok(Ok(value)) => output = Some(value),
                Ok(Err(error)) => return Err(error),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err("Command stdout reader disconnected".into());
                }
            }
        } else {
            std::thread::sleep(wait);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    #[cfg(target_os = "linux")]
    use std::time::Instant;

    #[cfg(target_os = "linux")]
    fn script(script: &str, stop: &AtomicBool, timeout: Duration) -> Result<String, String> {
        run(OsStr::new("/bin/sh"), &["-c", script], stop, timeout)
    }
    #[cfg(target_os = "windows")]
    fn script(script: &str, stop: &AtomicBool, timeout: Duration) -> Result<String, String> {
        run(
            OsStr::new("powershell.exe"),
            &["-NoProfile", "-NonInteractive", "-Command", script],
            stop,
            timeout,
        )
    }
    #[cfg(target_os = "windows")]
    #[test]
    fn command_kill_job_handle_is_not_inherited() {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::{GetHandleInformation, HANDLE_FLAG_INHERIT};
        let job = KillJob::new().unwrap();
        let mut flags = 0;
        assert_ne!(
            unsafe { GetHandleInformation(job.0.as_raw_handle(), &mut flags) },
            0
        );
        assert_eq!(flags & HANDLE_FLAG_INHERIT, 0);
    }

    #[test]
    fn child_lifetime_fixture() {
        if std::env::var_os("PC_ACTIVITY_COMMAND_CHILD_FIXTURE").is_some() {
            std::thread::sleep(Duration::from_secs(2));
        }
    }
    #[test]
    fn parent_exit_fixture() {
        let Some(marker) = std::env::var_os("PC_ACTIVITY_COMMAND_PARENT_MARKER") else {
            return;
        };
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "command::tests::child_lifetime_fixture",
                "--test-threads=1",
            ])
            .env("PC_ACTIVITY_COMMAND_CHILD_FIXTURE", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = spawn_owned(&mut command).expect("fixture child spawn");
        let pid = child.child.id();
        let token = crate::process_details::process_lifetime(pid).expect("fixture child lifetime");
        std::fs::write(marker, format!("{pid} {token}")).unwrap();
        // Deliberately skip Rust destructors, as process exit skips detached
        // sampler destructors in the real GUI. The kernel must stop this child.
        std::process::exit(0);
    }
    fn fixture_is_running(pid: u32, token: u64) -> bool {
        if crate::process_details::process_lifetime(pid) != Some(token) {
            return false;
        }
        #[cfg(target_os = "linux")]
        {
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|text| {
                    text.rsplit_once(") ")
                        .map(|(_, tail)| !tail.starts_with('Z'))
                })
                .unwrap_or(false)
        }
        #[cfg(target_os = "windows")]
        {
            use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
            use windows_sys::Win32::System::Threading::{
                OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
            };
            let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
            if handle.is_null() {
                return false;
            }
            let running = unsafe { WaitForSingleObject(handle, 0) == WAIT_TIMEOUT };
            unsafe {
                CloseHandle(handle);
            }
            running
        }
    }
    fn cleanup_fixture_child(pid: u32, token: u64) {
        if !fixture_is_running(pid, token) {
            return;
        }
        #[cfg(target_os = "linux")]
        unsafe {
            unsafe extern "C" {
                fn kill(pid: i32, signal: i32) -> i32;
            }
            kill(pid as i32, 9);
        }
        #[cfg(target_os = "windows")]
        unsafe {
            use windows_sys::Win32::Foundation::CloseHandle;
            use windows_sys::Win32::System::Threading::{
                OpenProcess, PROCESS_TERMINATE, TerminateProcess,
            };
            let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if !handle.is_null() {
                TerminateProcess(handle, 1);
                CloseHandle(handle);
            }
        }
    }
    #[test]
    fn native_parent_exit_stops_only_its_owned_command_without_rust_drop() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("owned-command-pid");
        let mut parent = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "command::tests::parent_exit_fixture",
                "--test-threads=1",
            ])
            .env("PC_ACTIVITY_COMMAND_PARENT_MARKER", &marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let status = loop {
            if let Some(status) = parent.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                let _ = parent.kill();
                let _ = parent.wait();
                panic!("fixture parent failed to exit");
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(status.success());
        let data = std::fs::read_to_string(marker).unwrap();
        let mut fields = data.split_whitespace();
        let pid = fields.next().unwrap().parse::<u32>().unwrap();
        let token = fields.next().unwrap().parse::<u64>().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_millis(300);
        while fixture_is_running(pid, token) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let survived = fixture_is_running(pid, token);
        cleanup_fixture_child(pid, token);
        assert!(!survived, "owned command survived its parent's exit");
    }

    #[test]
    fn stdout_accepts_256_kib_exactly_and_rejects_invalid_utf8() {
        assert_eq!(
            read_stdout(std::io::Cursor::new(vec![b'x'; 262_144]))
                .unwrap()
                .len(),
            262_144
        );
        assert!(
            read_stdout(std::io::Cursor::new(vec![b'x'; 262_145]))
                .unwrap_err()
                .contains("stdout limit")
        );
        assert!(
            read_stdout(std::io::Cursor::new([0xff]))
                .unwrap_err()
                .contains("UTF-8")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn owned_child_drop_reaps_its_process_instead_of_leaving_a_zombie() {
        unsafe extern "C" {
            fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
        }
        let mut command = Command::new("/bin/sleep");
        command
            .arg("2")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = OwnedChild {
            child: command.spawn().unwrap(),
            reaped: false,
        };
        let pid = child.child.id() as i32;
        assert!(child.child.try_wait().unwrap().is_none());
        drop(child);
        let mut status = 0;
        // WNOHANG=1 on Linux: a positive PID would mean this test, rather than
        // the production guard, reaped the zombie. ECHILD means it was reaped.
        let result = unsafe { waitpid(pid, &mut status, 1) };
        assert_eq!(result, -1);
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(10));
    }

    #[test]
    fn native_fixed_command_collects_output_and_rejects_failed_exit() {
        let stop = AtomicBool::new(false);
        #[cfg(target_os = "linux")]
        let output = script(
            "printf scanner-command-fixture",
            &stop,
            Duration::from_secs(3),
        )
        .unwrap();
        #[cfg(target_os = "windows")]
        let output = script(
            "[Console]::Out.Write('scanner-command-fixture')",
            &stop,
            Duration::from_secs(3),
        )
        .unwrap();
        assert_eq!(output, "scanner-command-fixture");
        assert!(
            script("exit 7", &stop, Duration::from_secs(3))
                .unwrap_err()
                .contains("status")
        );
    }
    #[test]
    fn native_stdout_limit_stops_the_owned_child() {
        let stop = AtomicBool::new(false);
        #[cfg(target_os = "linux")]
        let result = script("head -c 270000 /dev/zero", &stop, Duration::from_secs(3));
        #[cfg(target_os = "windows")]
        let result = script(
            "[Console]::Out.Write('x' * 270000)",
            &stop,
            Duration::from_secs(3),
        );
        assert!(result.unwrap_err().contains("stdout limit"));
    }
    #[test]
    fn native_timeout_kills_and_reaps_the_owned_child() {
        let stop = AtomicBool::new(false);
        #[cfg(target_os = "linux")]
        let result = script("exec sleep 2", &stop, Duration::from_millis(80));
        #[cfg(target_os = "windows")]
        let result = script("Start-Sleep -Seconds 2", &stop, Duration::from_millis(80));
        assert!(result.unwrap_err().contains("timed out"));
    }
    #[test]
    fn predicate_cancellation_before_spawn_does_not_start_any_program() {
        assert!(
            run_with_cancel(
                OsStr::new("nonexistent-command-fixture"),
                &[],
                &AtomicBool::new(false),
                Duration::from_secs(1),
                || true
            )
            .unwrap_err()
            .contains("cancelled")
        );
    }

    #[test]
    fn native_predicate_cancellation_interrupts_the_owned_child() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let setter = Arc::clone(&cancelled);
        let trigger = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            setter.store(true, Ordering::Relaxed);
        });
        let stop = AtomicBool::new(false);
        #[cfg(target_os = "linux")]
        let result = run_with_cancel(
            OsStr::new("/bin/sh"),
            &["-c", "exec sleep 2"],
            &stop,
            Duration::from_secs(3),
            || cancelled.load(Ordering::Relaxed),
        );
        #[cfg(target_os = "windows")]
        let result = run_with_cancel(
            OsStr::new("powershell.exe"),
            &[
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 2",
            ],
            &stop,
            Duration::from_secs(3),
            || cancelled.load(Ordering::Relaxed),
        );
        trigger.join().unwrap();
        assert!(result.unwrap_err().contains("cancelled"));
    }

    #[test]
    fn native_cancellation_interrupts_the_owned_child() {
        let stop = Arc::new(AtomicBool::new(false));
        let setter = Arc::clone(&stop);
        let trigger = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            setter.store(true, Ordering::Relaxed);
        });
        #[cfg(target_os = "linux")]
        let result = script("exec sleep 2", &stop, Duration::from_secs(3));
        #[cfg(target_os = "windows")]
        let result = script("Start-Sleep -Seconds 2", &stop, Duration::from_secs(3));
        trigger.join().unwrap();
        assert!(result.unwrap_err().contains("cancelled"));
    }
    #[test]
    fn cancellation_before_spawn_does_not_start_any_program() {
        assert!(
            run(
                OsStr::new("nonexistent-command-fixture"),
                &[],
                &AtomicBool::new(true),
                Duration::from_secs(1)
            )
            .unwrap_err()
            .contains("cancelled")
        );
    }
    #[test]
    fn reader_capacity_is_bounded_and_permits_are_released() {
        let counter = std::sync::atomic::AtomicUsize::new(0);
        let permits = (0..MAX_ACTIVE_READERS)
            .map(|_| ReaderPermit::acquire(&counter).unwrap())
            .collect::<Vec<_>>();
        assert!(ReaderPermit::acquire(&counter).is_none());
        drop(permits);
        assert_eq!(counter.load(Ordering::Relaxed), 0);
        assert!(ReaderPermit::acquire(&counter).is_some());
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn inherited_stdout_cannot_make_run_wait_for_a_grandchild() {
        let start = Instant::now();
        let result = script(
            "sleep 2 & exit 0",
            &AtomicBool::new(false),
            Duration::from_millis(80),
        );
        assert!(result.unwrap_err().contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
