//! Opt-in desktop delivery. Native APIs and subprocess work stay off the GUI.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;
#[cfg(target_os = "windows")]
use std::time::Instant;

const QUEUE_CAPACITY: usize = 8;
const TITLE_CHARS: usize = 128;
const BODY_CHARS: usize = 512;

#[derive(Clone, Debug, Default)]
pub struct DeliveryStatus {
    /// The OS API accepted the request, not proof it was visible to the user.
    pub accepted: u64,
    /// Queued jobs lost to capacity, cancellation, stale generation or OS error.
    pub dropped: u64,
    pub error: Option<String>,
}
struct Job {
    generation: u64,
    title: String,
    body: String,
}
struct Shared {
    // Low bit is enabled. Every actual transition advances the generation.
    // A disable/enable pair invalidates old jobs even between worker polls.
    config: AtomicU64,
    stop: Arc<AtomicBool>,
    status: Mutex<DeliveryStatus>,
    active: AtomicBool,
    exited: AtomicBool,
}
impl Shared {
    fn current(&self, job: &Job) -> bool {
        let state = self.config.load(Ordering::Acquire);
        state & 1 != 0 && state == job.generation && !self.stop.load(Ordering::Relaxed)
    }
    fn accepted(&self) {
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        status.accepted = status.accepted.saturating_add(1);
        status.error = None;
    }
    fn dropped(&self, error: Option<String>) {
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        status.dropped = status.dropped.saturating_add(1);
        if let Some(error) = error {
            status.error = Some(bounded_text(&error, 512));
        }
    }
}

#[derive(Clone)]
pub struct DesktopNotifications {
    shared: Arc<Shared>,
    sender: SyncSender<Job>,
}
impl DesktopNotifications {
    fn channel(stop: Arc<AtomicBool>) -> (Self, Receiver<Job>) {
        let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        (
            Self {
                shared: Arc::new(Shared {
                    config: AtomicU64::new(0),
                    stop,
                    status: Mutex::new(DeliveryStatus::default()),
                    active: AtomicBool::new(false),
                    exited: AtomicBool::new(false),
                }),
                sender,
            },
            receiver,
        )
    }
    pub fn start(stop: Arc<AtomicBool>) -> Self {
        let (service, receiver) = Self::channel(stop);
        let shared = Arc::clone(&service.shared);
        if let Err(error) = std::thread::Builder::new()
            .name("desktop-notifications".into())
            .spawn(move || worker(shared, receiver))
        {
            service.shared.dropped(Some(format!(
                "Не удалось запустить доставку уведомлений: {error}"
            )));
            service.shared.exited.store(true, Ordering::Release);
        }
        service
    }
    pub fn set_enabled(&self, value: bool) {
        let _ = self
            .shared
            .config
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                ((state & 1 != 0) != value).then(|| state.wrapping_add(1))
            });
    }
    /// Returns true only when queued. Disabled delivery is a normal no-op.
    /// It does not establish OS API acceptance or visible notification delivery.
    pub fn notify(&self, title: String, body: String) -> bool {
        let generation = self.shared.config.load(Ordering::Acquire);
        if generation & 1 == 0 || self.shared.stop.load(Ordering::Relaxed) {
            return false;
        }
        let job = Job {
            generation,
            title: bounded_text(&title, TITLE_CHARS),
            body: bounded_text(&body, BODY_CHARS),
        };
        match self.sender.try_send(job) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.shared.dropped(Some(
                    "Очередь уведомлений заполнена; событие остаётся в журнале приложения".into(),
                ));
                false
            }
            Err(TrySendError::Disconnected(_)) => {
                self.shared.dropped(Some(
                    "Служба доставки уведомлений недоступна; событие остаётся в журнале приложения"
                        .into(),
                ));
                false
            }
        }
    }
    /// True after the worker has released its own native notification resources.
    pub fn is_stopped(&self) -> bool {
        self.shared.exited.load(Ordering::Acquire)
    }

    pub fn status(&self) -> DeliveryStatus {
        self.shared
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

fn bounded_text(text: &str, limit: usize) -> String {
    text.chars()
        .take(limit)
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}
#[cfg(any(target_os = "linux", test))]
fn escape_markup(text: &str) -> String {
    let mut result = String::new();
    for ch in text.chars() {
        match ch {
            '&' => result.push_str("&amp;"),
            '<' => result.push_str("&lt;"),
            '>' => result.push_str("&gt;"),
            '\'' => result.push_str("&apos;"),
            '"' => result.push_str("&quot;"),
            ch if ch.is_control() => result.push(' '),
            ch => result.push(ch),
        }
    }
    result
}
#[cfg(any(target_os = "windows", test))]
fn utf16_array<const N: usize>(text: &str) -> [u16; N] {
    let mut result = [0_u16; N];
    let mut offset = 0;
    for ch in text.chars() {
        let mut buffer = [0_u16; 2];
        let encoded = ch.encode_utf16(&mut buffer);
        if offset + encoded.len() >= N {
            break;
        }
        result[offset..offset + encoded.len()].copy_from_slice(encoded);
        offset += encoded.len();
    }
    result
}
struct ActiveJob<'a>(&'a Shared);
impl Drop for ActiveJob<'_> {
    fn drop(&mut self) {
        self.0.active.store(false, Ordering::Release);
    }
}
fn process_job(
    shared: &Shared,
    job: Job,
    deliver: impl FnOnce(&Job, &Shared) -> Result<(), String>,
) {
    if !shared.current(&job) {
        shared.dropped(None);
        return;
    }
    shared.active.store(true, Ordering::Relaxed);
    let _active = ActiveJob(shared);
    if let Err(error) = deliver(&job, shared) {
        // Disabling notifications or stopping the app is not an OS failure.
        shared.dropped(shared.current(&job).then_some(error));
    }
}
fn worker(shared: Arc<Shared>, receiver: Receiver<Job>) {
    struct Exited(Arc<Shared>);
    impl Drop for Exited {
        fn drop(&mut self) {
            self.0.exited.store(true, Ordering::Release);
        }
    }
    let _exited = Exited(Arc::clone(&shared));
    while !shared.stop.load(Ordering::Relaxed) {
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(job) => process_job(&shared, job, platform::deliver),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    for _ in receiver.try_iter() {
        shared.dropped(None);
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    pub(super) fn deliver(job: &Job, shared: &Shared) -> Result<(), String> {
        if !shared.current(job) {
            shared.dropped(None);
            return Ok(());
        }
        let body = escape_markup(&job.body);
        // Direct argv: no shell, options cannot be injected by a process name.
        // The body markup subset is defined by freedesktop.org notifications.
        deliver_with_program(
            job,
            shared,
            std::ffi::OsStr::new("/usr/bin/notify-send"),
            &[
                "--app-name=PC Activity Scanner",
                "--expire-time=5000",
                "--urgency=normal",
                "--",
                &job.title,
                &body,
            ],
        )
    }
    pub(super) fn deliver_with_program(
        job: &Job,
        shared: &Shared,
        program: &std::ffi::OsStr,
        args: &[&str],
    ) -> Result<(), String> {
        crate::command::run_with_cancel(
            program,
            args,
            &shared.stop,
            Duration::from_secs(3),
            || !shared.current(job),
        )
        .map_err(|error| format!("notify-send недоступен: {error}"))?;
        if !shared.current(job) {
            shared.dropped(None);
            return Ok(());
        }
        shared.accepted();
        Ok(())
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use std::ptr;
    use windows_sys::Win32::UI::Shell::{
        NIF_ICON, NIF_INFO, NIF_TIP, NIIF_INFO, NIIF_NOSOUND, NIM_ADD, NIM_DELETE, NIM_MODIFY,
        NOTIFYICONDATAW, Shell_NotifyIconW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, DispatchMessageW, HWND_MESSAGE, IDI_APPLICATION, LoadIconW,
        MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
    };

    struct OwnedIcon {
        data: NOTIFYICONDATAW,
        added: bool,
    }
    impl Drop for OwnedIcon {
        fn drop(&mut self) {
            unsafe {
                if self.added {
                    let _ = Shell_NotifyIconW(NIM_DELETE, &self.data);
                }
                if !self.data.hWnd.is_null() {
                    let _ = DestroyWindow(self.data.hWnd);
                }
            }
        }
    }
    pub(super) fn deliver(job: &Job, shared: &Shared) -> Result<(), String> {
        if !shared.current(job) {
            shared.dropped(None);
            return Ok(());
        }
        // Built-in STATIC class, own message-only HWND. No GUI/tray private IDs.
        let class: [u16; 7] = [
            b'S' as u16,
            b'T' as u16,
            b'A' as u16,
            b'T' as u16,
            b'I' as u16,
            b'C' as u16,
            0,
        ];
        let name = utf16_array::<64>("PC Activity Scanner notifications");
        let window = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                name.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if window.is_null() {
            return Err(format!(
                "Окно доставки уведомлений недоступно: {}",
                std::io::Error::last_os_error()
            ));
        }
        let mut icon = OwnedIcon {
            data: NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: window,
                uID: 1,
                ..Default::default()
            },
            added: false,
        };
        icon.data.hIcon = unsafe { LoadIconW(ptr::null_mut(), IDI_APPLICATION) };
        if icon.data.hIcon.is_null() {
            return Err("Не удалось получить стандартный значок уведомления".into());
        }
        icon.data.uFlags = NIF_ICON | NIF_TIP;
        icon.data.szTip = utf16_array("PC Activity Scanner");
        if !shared.current(job) {
            shared.dropped(None);
            return Ok(());
        }
        if unsafe { Shell_NotifyIconW(NIM_ADD, &icon.data) } == 0 {
            return Err("Windows не приняла значок доставки уведомлений".into());
        }
        icon.added = true;
        icon.data.uFlags = NIF_INFO;
        icon.data.szInfoTitle = utf16_array(&job.title);
        icon.data.szInfo = utf16_array(&job.body);
        icon.data.dwInfoFlags = NIIF_INFO | NIIF_NOSOUND;
        icon.data.Anonymous.uTimeout = 5000;
        if !shared.current(job) {
            shared.dropped(None);
            return Ok(());
        }
        if unsafe { Shell_NotifyIconW(NIM_MODIFY, &icon.data) } == 0 {
            return Err("Windows не приняла запрос уведомления".into());
        }
        shared.accepted();
        // API acceptance is not visual proof: quiet hours, notification policy
        // and Explorer may suppress it. Never alter those user/system settings.
        let until = Instant::now();
        while until.elapsed() < Duration::from_secs(5) && shared.current(job) {
            let mut message: MSG = unsafe { std::mem::zeroed() };
            while unsafe { PeekMessageW(&mut message, window, 0, 0, PM_REMOVE) } != 0 {
                unsafe {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        // The worker drops only its own icon and HWND, including disable/stop.
        drop(icon);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    fn local_service() -> (DesktopNotifications, std::sync::mpsc::Receiver<Job>) {
        DesktopNotifications::channel(Arc::new(AtomicBool::new(false)))
    }
    #[test]
    fn disabled_worker_stops_without_os_calls_or_gui_wait() {
        let stop = Arc::new(AtomicBool::new(false));
        let service = DesktopNotifications::start(Arc::clone(&stop));
        assert!(!service.notify("disabled".into(), "body".into()));
        stop.store(true, Ordering::Relaxed);
        let deadline = Instant::now() + Duration::from_secs(1);
        while !service.is_stopped() {
            assert!(
                Instant::now() < deadline,
                "disabled notification worker failed to stop"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(service.status().accepted, 0);
        assert_eq!(service.status().error, None);
    }

    #[test]
    fn default_off_does_not_queue_or_invoke_delivery() {
        let (service, receiver) = local_service();
        assert!(!service.notify("title".into(), "body".into()));
        assert!(receiver.try_recv().is_err());
        assert_eq!(service.status().accepted, 0);
        assert_eq!(service.status().dropped, 0);
    }
    #[test]
    fn off_on_generation_discards_queued_jobs_before_delivery() {
        let (service, receiver) = local_service();
        service.set_enabled(true);
        assert!(service.notify("title".into(), "body".into()));
        let job = receiver.try_recv().unwrap();
        service.set_enabled(false);
        service.set_enabled(true);
        process_job(&service.shared, job, |_, _| {
            panic!("stale job reached OS delivery")
        });
        assert_eq!(service.status().accepted, 0);
        assert_eq!(service.status().dropped, 1);
        assert!(service.notify("fresh".into(), "body".into()));
        let job = receiver.try_recv().unwrap();
        service.set_enabled(true); // same setting must not invalidate this job.
        process_job(&service.shared, job, |_, shared| {
            shared.accepted();
            Ok(())
        });
        assert_eq!(service.status().accepted, 1);
    }
    #[test]
    fn queue_and_payloads_are_bounded_and_loss_is_reported() {
        let (service, receiver) = local_service();
        service.set_enabled(true);
        for _ in 0..8 {
            assert!(service.notify("🧪".repeat(1000), "界".repeat(2000)));
        }
        assert!(!service.notify("overflow".into(), "body".into()));
        assert_eq!(service.status().dropped, 1);
        let job = receiver.try_recv().unwrap();
        assert_eq!(job.title.chars().count(), 128);
        assert_eq!(job.body.chars().count(), 512);
    }
    #[test]
    fn markup_and_controls_are_text_not_notification_actions() {
        assert_eq!(
            escape_markup("<b>&'\"\0\n\t"),
            "&lt;b&gt;&amp;&apos;&quot;   "
        );
        assert_eq!(bounded_text("a\0b\nc", 128), "a b c");
    }
    #[test]
    fn utf16_buffers_never_split_non_bmp_characters() {
        let exact = utf16_array::<5>("🧪🧪x");
        assert_eq!(String::from_utf16(&exact[..4]).unwrap(), "🧪🧪");
        assert_eq!(exact[4], 0);
        let truncated = utf16_array::<4>("a🧪b");
        assert_eq!(String::from_utf16(&truncated[..3]).unwrap(), "a🧪");
        assert_eq!(truncated[3], 0);
    }
    #[test]
    fn stop_discards_jobs_and_delivery_errors_remain_visible() {
        let (service, receiver) = local_service();
        service.set_enabled(true);
        assert!(service.notify("title".into(), "body".into()));
        process_job(&service.shared, receiver.try_recv().unwrap(), |_, _| {
            Err("source unavailable".into())
        });
        assert_eq!(service.status().accepted, 0);
        assert_eq!(
            service.status().error.as_deref(),
            Some("source unavailable")
        );
        assert!(service.notify("title".into(), "body".into()));
        service.shared.stop.store(true, Ordering::Relaxed);
        process_job(&service.shared, receiver.try_recv().unwrap(), |_, _| {
            panic!("stopped delivery reached OS")
        });
        assert!(!service.notify("after stop".into(), "body".into()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_active_delivery_is_cancelled_on_disable_and_disable_enable_generation_change() {
        for reenable in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let marker = directory.path().join("notification-child-pid");
            let marker_arg = marker.to_string_lossy().into_owned();
            let (service, receiver) = local_service();
            service.set_enabled(true);
            assert!(service.notify("fixture".into(), "body".into()));
            let job = receiver.try_recv().unwrap();
            let shared = Arc::clone(&service.shared);
            let delivery = std::thread::spawn(move || {
                process_job(&shared, job, |job, shared| {
                    platform::deliver_with_program(
                        job,
                        shared,
                        std::ffi::OsStr::new("/bin/sh"),
                        &[
                            "-c",
                            "printf '%s' \"$$\" > \"$1\"; exec sleep 2",
                            "notification-fixture",
                            &marker_arg,
                        ],
                    )
                })
            });
            let started_deadline = Instant::now() + Duration::from_secs(1);
            let pid = loop {
                if let Some(pid) = std::fs::read_to_string(&marker)
                    .ok()
                    .and_then(|value| value.parse::<u32>().ok())
                {
                    break pid;
                }
                if Instant::now() >= started_deadline {
                    service.shared.stop.store(true, Ordering::Relaxed);
                    delivery.join().unwrap();
                    panic!("controlled notification child did not start");
                }
                std::thread::sleep(Duration::from_millis(5));
            };
            let token = crate::process_details::process_lifetime(pid)
                .expect("controlled child native lifetime");
            let cancelled_at = Instant::now();
            service.set_enabled(false);
            if reenable {
                service.set_enabled(true);
            }
            while !delivery.is_finished() {
                if cancelled_at.elapsed() >= Duration::from_millis(500) {
                    service.shared.stop.store(true, Ordering::Relaxed);
                    delivery.join().unwrap();
                    panic!(
                        "generation change did not cancel the active notification within 500ms (off/on={reenable})"
                    );
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            delivery.join().unwrap();
            assert_ne!(
                crate::process_details::process_lifetime(pid),
                Some(token),
                "owned notification child must be killed and reaped"
            );
            let status = service.status();
            assert_eq!(status.accepted, 0);
            assert_eq!(status.dropped, 1);
            assert_eq!(status.error, None);
            eprintln!(
                "Linux active notification cancelled in {:?}; off/on={reenable}; status={status:?}",
                cancelled_at.elapsed()
            );
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "shows one controlled native Windows balloon; requires PC_ACTIVITY_NOTIFICATION_SMOKE=1"]
    fn native_desktop_notification_smoke() {
        assert_eq!(
            std::env::var("PC_ACTIVITY_NOTIFICATION_SMOKE").as_deref(),
            Ok("1")
        );
        let stop = Arc::new(AtomicBool::new(false));
        let service = DesktopNotifications::start(Arc::clone(&stop));
        service.set_enabled(true);
        assert!(service.notify(
            "PC Activity Scanner: API test".into(),
            "Одна проверка локального уведомления. Принятие API не доказывает видимость.".into()
        ));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let status = service.status();
            assert!(
                status.error.is_none(),
                "Native notification failed: {status:?}"
            );
            if status.accepted == 1 && !service.shared.active.load(Ordering::Acquire) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Native notification did not finish: {status:?}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        eprintln!(
            "Windows Shell_NotifyIconW API accepted: {:?}; visual acceptance remains manual",
            service.status()
        );
        stop.store(true, Ordering::Relaxed);
        while !service.is_stopped() {
            assert!(
                Instant::now() < deadline,
                "notification worker did not exit"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "sends one local notification if DBus is available; requires PC_ACTIVITY_NOTIFICATION_SMOKE=1"]
    fn native_linux_notification_probe() {
        assert_eq!(
            std::env::var("PC_ACTIVITY_NOTIFICATION_SMOKE").as_deref(),
            Ok("1")
        );
        let stop = Arc::new(AtomicBool::new(false));
        let service = DesktopNotifications::start(Arc::clone(&stop));
        service.set_enabled(true);
        assert!(service.notify(
            "PC Activity Scanner: API test".into(),
            "Одна проверка локального notify-send. Видимость проверяется вручную.".into()
        ));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let status = service.status();
            if status.accepted > 0 || status.error.is_some() {
                eprintln!(
                    "Linux notification probe: {status:?}; API acceptance is not visual proof"
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "notification source did not respond"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        stop.store(true, Ordering::Relaxed);
        while !service.is_stopped() {
            assert!(
                Instant::now() < deadline,
                "notification worker did not exit"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}
