//! Opt-in elevated ETW producer. The GUI never samples or waits for UAC/ETW.
//! IPC accepts only bounded aggregate frames, never commands or packet contents.
use super::windows_protocol::{self as protocol, Frame, MAX_MESSAGE, MAX_PROCESSES, Traffic};
use super::{NetworkRate, NetworkSample};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, CoCreateGuid, CoInitializeEx, CoUninitialize,
};
use windows_sys::Win32::System::Diagnostics::Etw::*;
use windows_sys::Win32::System::Diagnostics::ToolHelp::*;

use windows_sys::Win32::System::Pipes::*;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;
use windows_sys::core::GUID;

const TCP: GUID = GUID::from_u128(0x9a280ac0_c8e0_11d1_84e2_00c04fb998a2);
const UDP: GUID = GUID::from_u128(0xbf3a50c5_a9c9_4988_a005_2df0b7c80f80);

struct Handle(HANDLE);
// Every owned handle is used by one worker; passing ownership does not alias it.
unsafe impl Send for Handle {}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
impl Handle {
    fn new(value: HANDLE) -> Result<Self, String> {
        if value.is_null() || value == INVALID_HANDLE_VALUE {
            Err(last_error())
        } else {
            Ok(Self(value))
        }
    }
}
fn last_error() -> String {
    std::io::Error::last_os_error().to_string()
}
fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn same_guid(a: GUID, b: GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}
fn alive(handle: HANDLE) -> bool {
    unsafe { WaitForSingleObject(handle, 0) == WAIT_TIMEOUT }
}
fn process(pid: u32) -> Result<Handle, String> {
    Handle::new(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid) })
}
fn creation(handle: HANDLE) -> Option<u64> {
    let (mut created, mut exited, mut kernel, mut user) =
        unsafe { (zeroed(), zeroed(), zeroed(), zeroed()) };
    if unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
        return None;
    }
    Some((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}
pub(crate) fn process_creation_time(pid: u32) -> Option<u64> {
    creation(process(pid).ok()?.0)
}
fn image(handle: HANDLE) -> Option<Vec<u16>> {
    let mut path = vec![0_u16; 32768];
    let mut len = path.len() as u32;
    if unsafe { QueryFullProcessImageNameW(handle, 0, path.as_mut_ptr(), &mut len) } == 0 {
        return None;
    }
    path.truncate(len as usize);
    Some(path)
}
fn new_guid() -> Result<GUID, String> {
    let mut guid = GUID::default();
    if unsafe { CoCreateGuid(&mut guid) } < 0 {
        return Err("CoCreateGuid failed".into());
    }
    Ok(guid)
}
fn guid_text(g: GUID) -> String {
    format!(
        "{:08x}{:04x}{:04x}{}",
        g.data1,
        g.data2,
        g.data3,
        g.data4
            .iter()
            .map(|v| format!("{v:02x}"))
            .collect::<String>()
    )
}
fn pipe_name(parent: u32, nonce: &str) -> Vec<u16> {
    wide(format!(r"\\.\pipe\pc-activity-{parent}-{nonce}"))
}

pub(super) struct WindowsSession {
    cancelled: Arc<AtomicBool>,
    session_name: Arc<Mutex<Option<Vec<u16>>>>,
}
impl WindowsSession {
    pub(super) fn start(state: Arc<Mutex<NetworkSample>>) -> Result<Self, String> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&cancelled);
        let session_name = Arc::new(Mutex::new(None));
        let worker_name = Arc::clone(&session_name);
        let epoch = state.lock().unwrap_or_else(|e| e.into_inner()).epoch;
        std::thread::Builder::new()
            .name("windows-traffic".into())
            .spawn(move || {
                if let Err(error) = client(&state, epoch, &stop, &worker_name) {
                    let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
                    if guard.epoch == epoch && !stop.load(Ordering::Acquire) {
                        guard.is_available = false;
                        guard.rates.clear();
                        guard.status = format!("Windows ETW: {error}");
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            cancelled,
            session_name,
        })
    }
    pub(super) fn stop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}
impl Drop for WindowsSession {
    fn drop(&mut self) {
        self.stop();
    }
}

fn create_pipe(name: &[u16]) -> Result<Handle, String> {
    // Owner and elevated administrators only; reject remote clients independently.
    let sddl = wide("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)");
    let mut descriptor = null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(last_error());
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let pipe = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            MAX_MESSAGE as u32,
            MAX_MESSAGE as u32,
            0,
            &attributes,
        )
    };
    unsafe {
        LocalFree(descriptor);
    }
    Handle::new(pipe)
}

fn launch(parent: u32, nonce: &str) -> Result<Handle, String> {
    let executable = wide(std::env::current_exe().map_err(|e| e.to_string())?);
    let parameters = wide(format!("--windows-traffic-helper {parent} {nonce}"));
    let verb = wide("runas");
    let apartment = unsafe { CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32) };
    let mut info: SHELLEXECUTEINFOW = unsafe { zeroed() };
    info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI;
    info.lpVerb = verb.as_ptr();
    info.lpFile = executable.as_ptr();
    info.lpParameters = parameters.as_ptr();
    info.nShow = SW_HIDE;
    let success = unsafe { ShellExecuteExW(&mut info) };
    let error = last_error();
    if apartment >= 0 {
        unsafe {
            CoUninitialize();
        }
    }
    if success == 0 {
        Err(format!("запуск помощника отменён или недоступен: {error}"))
    } else {
        Handle::new(info.hProcess)
    }
}

fn client(
    state: &Arc<Mutex<NetworkSample>>,
    epoch: u64,
    stop: &Arc<AtomicBool>,
    session_name: &Mutex<Option<Vec<u16>>>,
) -> Result<(), String> {
    let parent = unsafe { GetCurrentProcessId() };
    let nonce = guid_text(new_guid()?);
    *session_name.lock().unwrap_or_else(|e| e.into_inner()) =
        Some(wide(format!("PCActivity-{nonce}")));
    let name = pipe_name(parent, &nonce);
    let pipe = create_pipe(&name)?;
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("windows-traffic-uac".into())
        .spawn(move || {
            let _ = sender.send(launch(parent, &nonce));
        })
        .map_err(|e| e.to_string())?;
    let mut helper = None;
    let started = Instant::now();
    let mut connected = false;
    let mut authenticated = false;
    let mut buffer = vec![0_u8; MAX_MESSAGE];
    loop {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        if helper.is_none() {
            match receiver.try_recv() {
                Ok(result) => helper = Some(result?),
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err("поток запуска завершился".into());
                }
                Err(mpsc::TryRecvError::Empty) => (),
            }
        }
        if !connected {
            let success = unsafe { ConnectNamedPipe(pipe.0, null_mut()) };
            let error = unsafe { GetLastError() };
            connected = success != 0 || error == ERROR_PIPE_CONNECTED;
            if !connected && error != ERROR_PIPE_LISTENING && error != ERROR_NO_DATA {
                return Err(last_error());
            }
        }
        if connected
            && !authenticated
            && let Some(helper) = &helper
        {
            let mut peer = 0;
            if unsafe { GetNamedPipeClientProcessId(pipe.0, &mut peer) } == 0
                || peer != unsafe { GetProcessId(helper.0) }
            {
                return Err("отклонён посторонний клиент канала".into());
            }
            authenticated = true;
        }
        if authenticated {
            let mut read = 0;
            if unsafe {
                ReadFile(
                    pipe.0,
                    buffer.as_mut_ptr(),
                    buffer.len() as u32,
                    &mut read,
                    null_mut(),
                )
            } != 0
            {
                let data = &buffer[..read as usize];
                if data.starts_with(b"PCATERR1") {
                    return Err(String::from_utf8_lossy(&data[8..]).into_owned());
                }
                let frame = protocol::decode(data).ok_or("некорректный кадр помощника")?;
                if frame.lost != 0 {
                    return Err(format!(
                        "потеряно событий: {}; учёт остановлен, данные неполные",
                        frame.lost
                    ));
                }
                let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
                if guard.epoch != epoch {
                    return Ok(());
                }
                guard.publish_windows_frame(frame);
            } else if unsafe { GetLastError() } != ERROR_NO_DATA {
                return Err("помощник закрыл канал".into());
            }
        }
        if helper.as_ref().is_some_and(|p| !alive(p.0)) {
            return Err("помощник завершился; проверьте права ETW".into());
        }
        if !authenticated && started.elapsed() > Duration::from_secs(180) {
            return Err("истекло время ожидания UAC/помощника".into());
        }
        std::thread::sleep(Duration::from_millis(40));
    }
}

impl NetworkSample {
    fn publish_windows_frame(&mut self, frame: Frame) {
        let seconds = frame.elapsed_ns as f64 / 1e9;
        let mut rates = HashMap::with_capacity(frame.processes.len());
        let mut lifetimes = HashMap::with_capacity(frame.processes.len());
        let mut bytes = HashMap::with_capacity(frame.processes.len());
        for entry in frame.processes {
            // The unprivileged side independently verifies that the PID still denotes
            // the process observed by the producer. Unknown/protected processes stay unknown.
            if process_creation_time(entry.pid) != Some(entry.created) {
                continue;
            }
            rates.insert(
                entry.pid,
                NetworkRate {
                    upload_bps: entry.uploaded as f64 / seconds,
                    download_bps: entry.downloaded as f64 / seconds,
                },
            );
            lifetimes.insert(entry.pid, entry.created);
            bytes.insert(entry.pid, (entry.uploaded, entry.downloaded));
        }
        self.publish_timed_frame(
            rates,
            frame.captured_at,
            seconds,
            "Windows ETW · TCP/UDP IPv4/IPv6 · данные по PID",
        );
        self.process_lifetimes = lifetimes.clone();
        if let Some(pending) = self.pending.back_mut() {
            pending.process_lifetimes = lifetimes;
            pending.exact_bytes = bytes;
        }
    }
}

/// Called before initializing eframe, tray, settings, or any storage.
pub fn run_helper_if_requested() -> bool {
    let args = std::env::args().collect::<Vec<_>>();
    if args.get(1).map(String::as_str) != Some("--windows-traffic-helper") {
        return false;
    }
    let code = match args.as_slice() {
        [_, _, parent, nonce]
            if nonce.len() == 32 && nonce.bytes().all(|v| v.is_ascii_hexdigit()) =>
        {
            parent
                .parse::<u32>()
                .ok()
                .and_then(|pid| run_helper(pid, nonce).ok())
                .map_or(1, |_| 0)
        }
        _ => 2,
    };
    std::process::exit(code);
}

// Identification prevents the low-privilege pipe server from impersonating
// the elevated helper and using its token to perform arbitrary admin actions.
fn open_pipe(name: &[u16]) -> Result<Handle, String> {
    Handle::new(unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE | FILE_READ_ATTRIBUTES,
            0,
            null(),
            OPEN_EXISTING,
            SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            null_mut(),
        )
    })
}

fn run_helper(parent_pid: u32, nonce: &str) -> Result<(), String> {
    let parent = process(parent_pid)?;
    if !alive(parent.0)
        || image(parent.0) != image(unsafe { GetCurrentProcess() })
        || image(parent.0).is_none()
    {
        return Err("invalid parent executable".into());
    }
    let name = pipe_name(parent_pid, nonce);
    let pipe = open_pipe(&name)?;
    let mut server = 0;
    if unsafe { GetNamedPipeServerProcessId(pipe.0, &mut server) } == 0 || server != parent_pid {
        return Err("invalid pipe server".into());
    }
    let mode = PIPE_NOWAIT;
    if unsafe { SetNamedPipeHandleState(pipe.0, &mode, null(), null()) } == 0 {
        return Err(last_error());
    }
    let result = capture(&parent, &pipe, nonce);
    if let Err(error) = &result {
        let mut message = b"PCATERR1".to_vec();
        message.extend_from_slice(error.as_bytes().get(..1024).unwrap_or(error.as_bytes()));
        let mut written = 0;
        unsafe {
            WriteFile(
                pipe.0,
                message.as_ptr(),
                message.len() as u32,
                &mut written,
                null_mut(),
            );
        }
    }
    result
}

fn capture(parent: &Handle, pipe: &Handle, nonce: &str) -> Result<(), String> {
    let mut session = EtwSession::start(nonce)?;
    let mut previous = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let mut available = 0;
        if !alive(parent.0)
            || unsafe {
                PeekNamedPipe(
                    pipe.0,
                    null_mut(),
                    0,
                    null_mut(),
                    &mut available,
                    null_mut(),
                )
            } == 0
        {
            break;
        }
        if previous.elapsed() < Duration::from_secs(2) {
            continue;
        }
        if session.consumer.is_finished() {
            return Err("ETW consumer stopped".into());
        }
        // Flush before the frame boundary so buffered events don't silently become
        // two-second zero samples. ETW has nonzero delivery latency even after flush.
        session.query(EVENT_TRACE_CONTROL_FLUSH)?;
        let now = Instant::now();
        let elapsed_ns = now
            .duration_since(previous)
            .as_nanos()
            .min(u64::MAX as u128) as u64;
        previous = now;
        let mut state = session.state.lock().unwrap_or_else(|e| e.into_inner());
        state.refresh_processes();
        let processes = state.counts.values().copied().collect::<Vec<_>>();
        let lost = state.lost.saturating_add(session.events_lost());
        for value in state.counts.values_mut() {
            value.uploaded = 0;
            value.downloaded = 0;
        }
        drop(state);
        let frame = Frame {
            elapsed_ns,
            captured_at: unix_now(),
            lost,
            processes,
        };
        let data = protocol::encode(&frame).ok_or("frame too large")?;
        let mut written = 0;
        if unsafe {
            WriteFile(
                pipe.0,
                data.as_ptr(),
                data.len() as u32,
                &mut written,
                null_mut(),
            )
        } == 0
            || written as usize != data.len()
        {
            break;
        }
        if lost != 0 {
            break;
        }
    }
    Ok(())
}

struct EventState {
    // Handles pin lifetimes and permit a cheap exit check. Never infer identity from PID alone.
    processes: HashMap<u32, (Handle, u64)>,
    counts: HashMap<u32, Traffic>,
    lost: u64,
}
impl EventState {
    fn identity(&mut self, pid: u32) -> Option<u64> {
        if self
            .processes
            .get(&pid)
            .is_some_and(|(handle, _)| !alive(handle.0))
        {
            self.processes.remove(&pid);
            self.counts.remove(&pid);
        }
        if !self.processes.contains_key(&pid) {
            if self.processes.len() == MAX_PROCESSES {
                self.lost = self.lost.saturating_add(1);
                return None;
            }
            let handle = process(pid).ok()?;
            let created = creation(handle.0)?;
            self.processes.insert(pid, (handle, created));
            self.counts.insert(
                pid,
                Traffic {
                    pid,
                    created,
                    uploaded: 0,
                    downloaded: 0,
                },
            );
        }
        self.processes.get(&pid).map(|(_, created)| *created)
    }
    fn refresh_processes(&mut self) {
        self.processes.retain(|pid, (handle, _)| {
            if alive(handle.0) {
                true
            } else {
                self.counts.remove(pid);
                false
            }
        });
        let Ok(snapshot) = Handle::new(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) })
        else {
            return;
        };
        let mut entry: PROCESSENTRY32W = unsafe { zeroed() };
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut success = unsafe { Process32FirstW(snapshot.0, &mut entry) };
        while success != 0 {
            if entry.th32ProcessID != 0 {
                self.identity(entry.th32ProcessID);
            }
            success = unsafe { Process32NextW(snapshot.0, &mut entry) };
        }
    }
}

unsafe extern "system" fn event(record: *mut EVENT_RECORD) {
    // ETW owns the record and its payload for this callback only.
    let record = unsafe { &*record };
    if !same_guid(record.EventHeader.ProviderId, TCP)
        && !same_guid(record.EventHeader.ProviderId, UDP)
    {
        return;
    }
    let descriptor = record.EventHeader.EventDescriptor;
    if !matches!(descriptor.Opcode, 10 | 11 | 26 | 27) {
        return;
    }
    let state = unsafe { &*(record.UserContext as *const Mutex<EventState>) };
    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
    if record.UserData.is_null() {
        state.lost = state.lost.saturating_add(1);
        return;
    }
    let data = unsafe {
        std::slice::from_raw_parts(record.UserData as *const u8, record.UserDataLength as usize)
    };
    let Some((pid, size, upload)) =
        protocol::parse_ip_event(descriptor.Version, descriptor.Opcode, data)
    else {
        state.lost = state.lost.saturating_add(1);
        return;
    };
    if pid == 0 {
        return;
    }
    let Some(created) = state.identity(pid) else {
        return;
    }; // Explicit unknown PID, never a global zero.
    // ProcessTrace converts timestamps to FILETIME unless RAW_TIMESTAMP is requested.
    if record.EventHeader.TimeStamp < 0 || created > record.EventHeader.TimeStamp as u64 {
        return;
    }
    if let Some(count) = state.counts.get_mut(&pid) {
        if upload {
            count.uploaded = count.uploaded.saturating_add(u64::from(size));
        } else {
            count.downloaded = count.downloaded.saturating_add(u64::from(size));
        }
    }
}

#[repr(C)]
struct Properties {
    properties: EVENT_TRACE_PROPERTIES,
    name: [u16; 128],
}
struct EtwSession {
    handle: CONTROLTRACE_HANDLE,
    properties: Box<Properties>,
    trace: PROCESSTRACE_HANDLE,
    consumer: std::thread::JoinHandle<()>,
    state: Arc<Mutex<EventState>>,
}
impl EtwSession {
    fn start(nonce: &str) -> Result<Self, String> {
        let mut properties: Box<Properties> = Box::new(unsafe { zeroed() });
        let name = wide(format!("PCActivity-{nonce}"));
        properties.name[..name.len()].copy_from_slice(&name);
        let p = &mut properties.properties;
        p.Wnode.BufferSize = size_of::<Properties>() as u32;
        p.Wnode.Guid = new_guid()?; // Never hijack the global NT Kernel Logger.
        p.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        p.Wnode.ClientContext = 1; // QPC internally; ProcessTrace returns FILETIME.
        p.BufferSize = 64;
        p.MinimumBuffers = 16;
        p.MaximumBuffers = 64;
        p.LogFileMode = EVENT_TRACE_REAL_TIME_MODE | EVENT_TRACE_SYSTEM_LOGGER_MODE;
        p.FlushTimer = 1;
        p.EnableFlags = EVENT_TRACE_FLAG_NETWORK_TCPIP;
        p.LoggerNameOffset = size_of::<EVENT_TRACE_PROPERTIES>() as u32;
        let mut handle = CONTROLTRACE_HANDLE { Value: 0 };
        let result = unsafe {
            StartTraceW(
                &mut handle,
                properties.name.as_ptr(),
                &mut properties.properties,
            )
        };
        if result != ERROR_SUCCESS {
            return Err(format!("StartTraceW: {result}"));
        }
        let state = Arc::new(Mutex::new(EventState {
            processes: HashMap::new(),
            counts: HashMap::new(),
            lost: 0,
        }));
        state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .refresh_processes();
        let mut logfile: EVENT_TRACE_LOGFILEW = unsafe { zeroed() };
        logfile.LoggerName = properties.name.as_mut_ptr();
        logfile.Anonymous1.ProcessTraceMode =
            PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD;
        logfile.Anonymous2.EventRecordCallback = Some(event);
        logfile.Context = Arc::as_ptr(&state) as *mut _;
        let trace = unsafe { OpenTraceW(&mut logfile) };
        if trace.Value == u64::MAX {
            let error = last_error();
            unsafe {
                ControlTraceW(
                    handle,
                    null(),
                    &mut properties.properties,
                    EVENT_TRACE_CONTROL_STOP,
                );
            }
            return Err(error);
        }
        let thread_state = Arc::clone(&state);
        let trace_value = trace.Value;
        let consumer = std::thread::Builder::new()
            .name("windows-etw-events".into())
            .spawn(move || {
                let _keep_alive = thread_state;
                let trace = PROCESSTRACE_HANDLE { Value: trace_value };
                unsafe {
                    ProcessTrace(&trace, 1, null(), null());
                }
            });
        let consumer = match consumer {
            Ok(thread) => thread,
            Err(error) => {
                // StartTrace already owns a live kernel session at this point.
                unsafe {
                    ControlTraceW(
                        handle,
                        null(),
                        &mut properties.properties,
                        EVENT_TRACE_CONTROL_STOP,
                    );
                    CloseTrace(trace);
                }
                return Err(error.to_string());
            }
        };
        Ok(Self {
            handle,
            properties,
            trace,
            consumer,
            state,
        })
    }
    fn query(&mut self, operation: u32) -> Result<(), String> {
        let result = unsafe {
            ControlTraceW(
                self.handle,
                null(),
                &mut self.properties.properties,
                operation,
            )
        };
        if result == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!("ControlTraceW: {result}"))
        }
    }
    fn events_lost(&self) -> u64 {
        let p = &self.properties.properties;
        u64::from(p.EventsLost).saturating_add(u64::from(p.RealTimeBuffersLost))
    }
}
impl Drop for EtwSession {
    fn drop(&mut self) {
        // Only a handle returned by our successful StartTrace is ever stopped.
        let _ = self.query(EVENT_TRACE_CONTROL_STOP);
        unsafe {
            CloseTrace(self.trace);
        }
        // ProcessTrace retains state ownership until its callback exits. No GUI wait.
    }
}

/// CI acceptance must be explicitly requested and run from an already elevated
/// process: automated tests must never wait for a UAC prompt.
pub fn run_smoke_test() -> Result<(), String> {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream, UdpSocket};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    let mut token = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(last_error());
    }
    let token = Handle::new(token)?;
    let mut elevation: TOKEN_ELEVATION = unsafe { zeroed() };
    let mut size = 0;
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            &mut elevation as *mut _ as *mut _,
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        )
    } == 0
        || elevation.TokenIsElevated == 0
    {
        return Err("SKIP: Windows ETW smoke test requires an already elevated process".into());
    }
    let mut backend = super::NetworkBackend::default();
    backend.enable()?;
    let state = backend.shared();
    let pid = unsafe { GetCurrentProcessId() };
    let collect = |phase: &str| -> Result<((u64, u64), usize), String> {
        let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
        if !guard.is_available {
            return Err(guard.status.clone());
        }
        let frames = guard.take_frames();
        drop(guard);
        let mut bytes = protocol::ByteProgress::default();
        for frame in &frames {
            let delta = frame.exact_bytes.get(&pid).copied().unwrap_or_default();
            bytes.add(delta);
            println!(
                "ETW {phase} frame: epoch={}, sequence={}, captured_at={}, seconds={:.3}, self_up={}, self_down={}",
                frame.epoch,
                frame.sequence,
                frame.captured_at,
                frame.seconds_per_sample,
                delta.0,
                delta.1
            );
        }
        Ok(((bytes.uploaded, bytes.downloaded), frames.len()))
    };
    let result = (|| {
        let wait_ready = Instant::now();
        loop {
            {
                let guard = state.lock().unwrap_or_else(|e| e.into_inner());
                if guard.is_available {
                    break;
                }
                if !guard.status.contains("ожидание") {
                    return Err(guard.status.clone());
                }
            }
            if wait_ready.elapsed() > Duration::from_secs(20) {
                return Err("ETW helper did not become ready".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let payload = vec![0x5a; 16_384];
        let mut received = vec![0; payload.len()];
        for host in ["127.0.0.1", "::1"] {
            let address = if host.contains(':') {
                format!("[{host}]:0")
            } else {
                format!("{host}:0")
            };
            for tcp in [true, false] {
                // The previous phase already reached its full expected byte count.
                // Discard its quiet tail before generating a different protocol.
                std::thread::sleep(Duration::from_secs(4));
                collect("completed-phase tail")?;
                let started = Instant::now();
                let mut sent = 0_u64;
                if tcp {
                    let listener = TcpListener::bind(&address).map_err(|e| e.to_string())?;
                    let mut sender =
                        TcpStream::connect(listener.local_addr().map_err(|e| e.to_string())?)
                            .map_err(|e| e.to_string())?;
                    let (mut receiver, _) = listener.accept().map_err(|e| e.to_string())?;
                    sender
                        .set_write_timeout(Some(Duration::from_secs(2)))
                        .map_err(|e| e.to_string())?;
                    receiver
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .map_err(|e| e.to_string())?;
                    while started.elapsed() < Duration::from_secs(3) {
                        sender.write_all(&payload).map_err(|e| e.to_string())?;
                        receiver
                            .read_exact(&mut received)
                            .map_err(|e| e.to_string())?;
                        sent += payload.len() as u64;
                        std::thread::sleep(Duration::from_millis(5));
                    }
                } else {
                    let sender = UdpSocket::bind(&address).map_err(|e| e.to_string())?;
                    let receiver = UdpSocket::bind(&address).map_err(|e| e.to_string())?;
                    receiver
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .map_err(|e| e.to_string())?;
                    while started.elapsed() < Duration::from_secs(3) {
                        sender
                            .send_to(
                                &payload[..1024],
                                receiver.local_addr().map_err(|e| e.to_string())?,
                            )
                            .map_err(|e| e.to_string())?;
                        let len = receiver
                            .recv_from(&mut received)
                            .map_err(|e| e.to_string())?
                            .0;
                        if len != 1024 {
                            return Err("UDP test datagram was truncated".into());
                        }
                        sent += 1024;
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
                // Flush, ProcessTrace callbacks, frame publication and IPC are
                // asynchronous. A fixed sleep cannot establish completion.
                let waiting = Instant::now();
                let phase = format!("{} {host}", if tcp { "TCP" } else { "UDP" });
                let mut progress = protocol::ByteProgress::default();
                loop {
                    let (delta, frame_count) = collect(&phase)?;
                    progress.add(delta);
                    if frame_count != 0 {
                        println!(
                            "ETW {phase} progress: wait={:.3}s, uploaded={}, downloaded={}, expected={sent}",
                            waiting.elapsed().as_secs_f64(),
                            progress.uploaded,
                            progress.downloaded
                        );
                    }
                    if progress.complete(sent) {
                        break;
                    }
                    if waiting.elapsed() >= Duration::from_secs(12) {
                        return Err(format!(
                            "{phase}: ETW self-PID {pid} counted up={}, down={}, expected >= {sent} bytes each after {:.3}s completion deadline",
                            progress.uploaded,
                            progress.downloaded,
                            waiting.elapsed().as_secs_f64()
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                let uploaded = progress.uploaded;
                let downloaded = progress.downloaded;
                println!(
                    "ETW {} {host}: sent={sent}, uploaded={uploaded}, downloaded={downloaded}",
                    if tcp { "TCP" } else { "UDP" }
                );
            }
        }
        Ok(())
    })();
    let session_name = backend.session.as_ref().and_then(|s| {
        s.session_name
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    });
    backend.disable();
    // Cancellation precedes every error return, including failed readiness.
    let Some(session_name) = session_name else {
        return result.and(Err("missing ETW session name".into()));
    };
    let stopped_at = Instant::now();
    loop {
        let mut properties: Properties = unsafe { zeroed() };
        properties.properties.Wnode.BufferSize = size_of::<Properties>() as u32;
        properties.properties.LoggerNameOffset = size_of::<EVENT_TRACE_PROPERTIES>() as u32;
        let status = unsafe {
            ControlTraceW(
                CONTROLTRACE_HANDLE { Value: 0 },
                session_name.as_ptr(),
                &mut properties.properties,
                EVENT_TRACE_CONTROL_QUERY,
            )
        };
        if status == ERROR_WMI_INSTANCE_NOT_FOUND {
            break;
        }
        if status != ERROR_SUCCESS {
            return Err(format!("ETW shutdown query failed: {status}"));
        }
        if stopped_at.elapsed() > Duration::from_secs(5) {
            return Err("owned ETW session survived backend disable".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Security::{
        GetTokenInformation, RevertToSelf, SecurityIdentification, TOKEN_QUERY,
        TokenImpersonationLevel,
    };

    #[test]
    fn pipe_verifies_peers_rejects_name_collision_and_cannot_delegate_helper_privileges() {
        let pid = unsafe { GetCurrentProcessId() };
        let name = pipe_name(pid, &guid_text(new_guid().unwrap()));
        let server = create_pipe(&name).unwrap();
        assert!(create_pipe(&name).is_err());
        let client = open_pipe(&name).unwrap();
        let mut actual_pid = 0;
        assert_ne!(
            unsafe { GetNamedPipeServerProcessId(client.0, &mut actual_pid) },
            0
        );
        assert_eq!(actual_pid, pid);
        assert_ne!(
            unsafe { GetNamedPipeClientProcessId(server.0, &mut actual_pid) },
            0
        );
        assert_eq!(actual_pid, pid);
        let byte = [1_u8];
        let mut written = 0;
        assert_ne!(
            unsafe { WriteFile(client.0, byte.as_ptr(), 1, &mut written, null_mut()) },
            0
        );
        let mut received = [0_u8];
        let mut read = 0;
        assert_ne!(
            unsafe { ReadFile(server.0, received.as_mut_ptr(), 1, &mut read, null_mut()) },
            0
        );
        assert_ne!(unsafe { ImpersonateNamedPipeClient(server.0) }, 0);
        // Always revert before assertions/panics; token inspection is the sole
        // action performed while impersonating at identification level.
        let mut token = null_mut();
        let opened = unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) };
        let mut level = -1_i32;
        let mut size = 0;
        let queried = if opened != 0 {
            unsafe {
                GetTokenInformation(
                    token,
                    TokenImpersonationLevel,
                    &mut level as *mut _ as *mut _,
                    size_of::<i32>() as u32,
                    &mut size,
                )
            }
        } else {
            0
        };
        let reverted = unsafe { RevertToSelf() };
        if !token.is_null() {
            unsafe {
                CloseHandle(token);
            }
        }
        assert_ne!(reverted, 0);
        assert_ne!(opened, 0);
        assert_ne!(queried, 0);
        assert_eq!(level, SecurityIdentification);
        drop(server);
        assert_eq!(
            unsafe { PeekNamedPipe(client.0, null_mut(), 0, null_mut(), null_mut(), null_mut()) },
            0
        );
    }
}
