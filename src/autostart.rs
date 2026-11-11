//! Per-user Windows logon task; settings are read from Task Scheduler.

fn literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn encoded_command(script: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<_> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut result = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = chunk[0] as usize;
        let b = chunk.get(1).copied().unwrap_or(0) as usize;
        let c = chunk.get(2).copied().unwrap_or(0) as usize;
        result.push(ALPHABET[a >> 2] as char);
        result.push(ALPHABET[((a & 3) << 4) | (b >> 4)] as char);
        result.push(if chunk.len() > 1 {
            ALPHABET[((b & 15) << 2) | (c >> 6)] as char
        } else {
            '='
        });
        result.push(if chunk.len() > 2 {
            ALPHABET[c & 63] as char
        } else {
            '='
        });
    }
    result
}

// Conservative ACL policy: only SYSTEM, Administrators and TrustedInstaller may
// replace the executable or any existing ancestor. No permissions are modified.
const PROTECTED_PATH: &str = r#"
function Assert-ProtectedExecutable([string]$path) {
    $root = [System.IO.Path]::GetPathRoot($path)
    if ($root -ine [System.IO.Path]::GetPathRoot([Environment]::SystemDirectory)) {
        throw 'Elevated startup requires a protected local installation on the Windows system drive.'
    }
    $trustedInstaller = ([System.Security.Principal.NTAccount]::new('NT SERVICE\TrustedInstaller')).Translate([System.Security.Principal.SecurityIdentifier]).Value
    $trusted = @('S-1-5-18', 'S-1-5-32-544', $trustedInstaller)
    $item = Get-Item -LiteralPath $path -Force
    if ($item.PSIsContainer) { throw 'Startup executable must be a file.' }
    $chain = [System.Collections.Generic.List[object]]::new()
    $chain.Add($item)
    $parent = $item.Directory
    while ($null -ne $parent) { $chain.Add($parent); $parent = $parent.Parent }
    $chain.Reverse()
    foreach ($entry in $chain) {
        if (($entry.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw 'Elevated startup refuses symbolic links, junctions and other reparse points.'
        }
        $acl = Get-Acl -LiteralPath $entry.FullName
        $owner = $acl.GetOwner([System.Security.Principal.SecurityIdentifier]).Value
        if ($trusted -notcontains $owner) {
            throw 'Elevated startup requires protected ownership. Install the executable in Program Files using administrator rights.'
        }
        $raw = [System.Security.AccessControl.RawSecurityDescriptor]::new($acl.GetSecurityDescriptorBinaryForm(), 0)
        if ($null -eq $raw.DiscretionaryAcl) { throw 'Elevated startup refuses a null DACL.' }
        # CreateFiles/WriteData, WriteEA, WriteAttributes, DeleteChild, Delete,
        # WriteDac, WriteOwner. CreateDirectories alone at C:\ is harmless for
        # this existing chain. AppendData is additionally forbidden on the file.
        $dangerous = 2 -bor 16 -bor 256 -bor 64 -bor 65536 -bor 262144 -bor 524288
        if ($entry -isnot [System.IO.DirectoryInfo]) { $dangerous = $dangerous -bor 4 }
        foreach ($rule in $acl.GetAccessRules($true, $true, [System.Security.Principal.SecurityIdentifier])) {
            if ($rule.AccessControlType -ne [System.Security.AccessControl.AccessControlType]::Allow -or
                ($rule.PropagationFlags -band [System.Security.AccessControl.PropagationFlags]::InheritOnly) -ne 0) { continue }
            if ($trusted -notcontains $rule.IdentityReference.Value -and
                ([int]$rule.FileSystemRights -band $dangerous) -ne 0) {
                throw 'Elevated startup refuses user-writable paths. Install the executable in protected Program Files; do not weaken its ACL.'
            }
        }
    }
}
"#;

const TASK_CONTEXT: &str = r#"
$service = New-Object -ComObject 'Schedule.Service'
$service.Connect()
$folder = $service.GetFolder('\')
$name = 'PCActivityScanner-' + $sid
$task = $null
try { $task = $folder.GetTask($name) } catch {
    $cause = $_.Exception
    while ($null -ne $cause.InnerException) { $cause = $cause.InnerException }
    if ($cause.HResult -ne -2147024894) { throw }
}
if ($null -ne $task) {
    $existing = $task.Definition
    if ($existing.RegistrationInfo.Source -cne 'dev.97dmitry.pc_activity_scanner.autostart.v1' -or
        $existing.Principal.UserId -ine $sid) {
        throw 'Startup task name belongs to another task; refusing to change it.'
    }
}
"#;

fn configuration_script(
    sid: &str,
    exe: &str,
    enabled: bool,
    elevated: bool,
    expires: u64,
) -> String {
    let mut script = format!(
        "$sid = {}\n$exe = {}\n$expires = {expires}\nif ([DateTimeOffset]::UtcNow.ToUnixTimeSeconds() -gt $expires) {{ throw 'Startup approval expired; retry.' }}\n",
        literal(sid),
        literal(exe)
    );
    script.push_str(TASK_CONTEXT);
    if !enabled {
        script.push_str("if ($null -ne $task) { $folder.DeleteTask($name, 0) }\n");
        return script;
    }
    if elevated {
        script.push_str(PROTECTED_PATH);
        script.push_str("Assert-ProtectedExecutable $exe\n");
    }
    script.push_str(&format!(r#"
$definition = $service.NewTask(0)
$definition.RegistrationInfo.Source = 'dev.97dmitry.pc_activity_scanner.autostart.v1'
$definition.RegistrationInfo.Description = 'PC Activity Scanner: interactive startup for the selected user.'
$definition.Principal.UserId = $sid
$definition.Principal.LogonType = 3
$definition.Principal.RunLevel = {}
$definition.Settings.Enabled = $true
$definition.Settings.DisallowStartIfOnBatteries = $false
$definition.Settings.StopIfGoingOnBatteries = $false
$definition.Settings.ExecutionTimeLimit = 'PT0S'
$definition.Settings.MultipleInstances = 2
$trigger = $definition.Triggers.Create(9)
$trigger.UserId = $sid
$trigger.Enabled = $true
$action = $definition.Actions.Create(0)
$action.Path = $exe
$action.WorkingDirectory = [System.IO.Path]::GetDirectoryName($exe)
$flags = 2
if ($null -ne $task) {{ $flags = 4 }}
$null = $folder.RegisterTaskDefinition($name, $definition, $flags, $sid, $null, 3, $null)
"#, u8::from(elevated)));
    script
}

#[cfg(target_os = "windows")]
mod windows {
    use super::{PROTECTED_PATH, TASK_CONTEXT, configuration_script, encoded_command, literal};
    use serde::Deserialize;
    use std::io::{Read, Write};
    use std::os::windows::{ffi::OsStringExt, process::CommandExt};
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    #[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
    pub struct State {
        pub enabled: bool,
        pub elevated: bool,
        pub needs_repair: bool,
    }

    #[derive(Deserialize)]
    struct Context {
        sid: String,
        administrator: bool,
        #[serde(flatten)]
        state: State,
    }

    enum Request {
        Query,
        Configure(State),
    }

    /// A single bounded background worker. No process wait occurs on the GUI thread.
    pub struct Controller {
        requests: SyncSender<Request>,
        results: Receiver<Result<State, String>>,
        busy: bool,
    }

    impl Default for Controller {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Controller {
        pub fn new() -> Self {
            let (requests, receive) = mpsc::sync_channel(1);
            let (send, results) = mpsc::sync_channel(1);
            std::thread::spawn(move || {
                while let Ok(request) = receive.recv() {
                    let result = match request {
                        Request::Query => query().map(|context| context.state),
                        Request::Configure(state) => configure(state),
                    };
                    if send.send(result).is_err() {
                        break;
                    }
                }
            });
            Self {
                requests,
                results,
                busy: false,
            }
        }

        pub fn query(&mut self) -> bool {
            self.request(Request::Query)
        }
        pub fn configure(&mut self, state: State) -> bool {
            self.request(Request::Configure(state))
        }
        pub fn is_busy(&self) -> bool {
            self.busy
        }

        fn request(&mut self, request: Request) -> bool {
            if self.busy {
                return false;
            }
            if self.requests.try_send(request).is_ok() {
                self.busy = true;
                true
            } else {
                false
            }
        }

        pub fn poll(&mut self) -> Option<Result<State, String>> {
            match self.results.try_recv() {
                Ok(result) => {
                    self.busy = false;
                    Some(result)
                }
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) if self.busy => {
                    self.busy = false;
                    Some(Err("Поток настройки автозапуска завершился".to_owned()))
                }
                Err(_) => None,
            }
        }
    }

    fn powershell_path() -> Result<PathBuf, String> {
        let mut buffer = vec![0u16; 32768];
        // SAFETY: the buffer is writable for the supplied number of UTF-16 units.
        let length = unsafe {
            windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW(
                buffer.as_mut_ptr(),
                buffer.len() as u32,
            )
        } as usize;
        if length == 0 || length >= buffer.len() {
            return Err("Не удалось найти системный каталог Windows".to_owned());
        }
        Ok(
            PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length]))
                .join("System32\\WindowsPowerShell\\v1.0\\powershell.exe"),
        )
    }

    fn execute(script: &str, timeout: Duration) -> Result<String, String> {
        let wrapped = format!(
            r#"
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
try {{
{script}
}} catch {{
    $message = $_.Exception.Message
    [Console]::Error.WriteLine($message.Substring(0, [Math]::Min(2048, $message.Length)))
    exit 1
}}
"#
        );
        let mut child = Command::new(powershell_path()?)
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-EncodedCommand",
                &encoded_command("[Console]::InputEncoding=[System.Text.UTF8Encoding]::new($false); & ([ScriptBlock]::Create([Console]::In.ReadToEnd()))"),
            ])
            .creation_flags(0x08000000) // CREATE_NO_WINDOW; UAC remains visible.
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("Не удалось запустить Windows PowerShell: {error}"))?;
        let payload = wrapped.into_bytes();
        let mut input = child.stdin.take().ok_or("Нет stdin PowerShell")?;
        let writer = std::thread::spawn(move || input.write_all(&payload));
        fn reader(stream: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                let _ = stream.take(16 * 1024).read_to_end(&mut bytes);
                bytes
            })
        }
        let output = reader(child.stdout.take().ok_or("Нет stdout PowerShell")?);
        let errors = reader(child.stderr.take().ok_or("Нет stderr PowerShell")?);
        let start = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if start.elapsed() < timeout => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(
                        "Истекло время настройки автозапуска. Проверьте состояние повторно."
                            .to_owned(),
                    );
                }
            }
        };
        let _ = writer.join();
        let stdout = output.join().unwrap_or_default();
        let stderr = errors.join().unwrap_or_default();
        if !status.success() {
            return Err(format!(
                "Ошибка автозапуска Windows: {}",
                String::from_utf8_lossy(&stderr).trim()
            ));
        }
        String::from_utf8(stdout).map_err(|_| "Некорректный ответ Windows PowerShell".to_owned())
    }

    fn identity_script() -> &'static str {
        r#"
$identity = [System.Security.Principal.WindowsIdentity]::GetCurrent()
$sid = $identity.User.Value
# Groups includes the Administrators SID in a filtered UAC token as well.
$administrator = @($identity.Groups | Where-Object { $_.Value -eq 'S-1-5-32-544' }).Count -gt 0
"#
    }

    fn query() -> Result<Context, String> {
        let exe = std::env::current_exe().map_err(|error| error.to_string())?;
        let exe = exe
            .to_str()
            .ok_or("Путь программы содержит некорректный Unicode")?;
        let script = format!(
            r#"
{}
$exe = {}
{}
$enabled = $false
$elevated = $false
$needsRepair = $false
{}
if ($null -ne $task) {{
    $definition = $task.Definition
    if ($definition.Principal.LogonType -ne 3 -or
        $definition.Actions.Count -ne 1 -or $definition.Triggers.Count -ne 1 -or
        $definition.Actions.Item(1).Type -ne 0 -or
        $definition.Actions.Item(1).Path -ine $exe -or
        -not [String]::IsNullOrEmpty($definition.Actions.Item(1).Arguments) -or
        $definition.Triggers.Item(1).Type -ne 9 -or
        $definition.Triggers.Item(1).UserId -ine $sid -or
        -not $definition.Triggers.Item(1).Enabled) {{
        $needsRepair = $true
    }}
    $enabled = [bool]$task.Enabled
    $elevated = $definition.Principal.RunLevel -eq 1
    if ($elevated) {{
        try {{ Assert-ProtectedExecutable $definition.Actions.Item(1).Path }} catch {{ $needsRepair = $true }}
    }}
}}
@{{sid=$sid; administrator=$administrator; enabled=$enabled; elevated=$elevated; needs_repair=$needsRepair}} | ConvertTo-Json -Compress
"#,
            identity_script(),
            literal(exe),
            TASK_CONTEXT,
            PROTECTED_PATH
        );
        let output = execute(&script, Duration::from_secs(30))?;
        serde_json::from_str(output.trim_start_matches('\u{feff}').trim())
            .map_err(|error| format!("Некорректное состояние автозапуска: {error}"))
    }

    fn configure(state: State) -> Result<State, String> {
        // Resolve identity before UAC. Alternate administrator credentials must not
        // redirect the task to the administrator's own logon session.
        let identity = execute(
            &format!(
                "{}\n@{{sid=$sid; administrator=$administrator; enabled=$false; elevated=$false; needs_repair=$false}} | ConvertTo-Json -Compress",
                identity_script()
            ),
            Duration::from_secs(30),
        )?;
        let context: Context = serde_json::from_str(identity.trim_start_matches('\u{feff}').trim())
            .map_err(|error| error.to_string())?;
        if state.enabled && state.elevated && !context.administrator {
            return Err("Автозапуск с правами администратора доступен только для исходной учётной записи администратора Windows. Другие учётные данные UAC не повышают права этой учётной записи.".to_owned());
        }
        let exe = std::env::current_exe().map_err(|error| error.to_string())?;
        let exe = exe
            .to_str()
            .ok_or("Путь программы содержит некорректный Unicode")?;
        if state.enabled && state.elevated {
            execute(
                &format!(
                    "{}\nAssert-ProtectedExecutable {}",
                    PROTECTED_PATH,
                    literal(exe)
                ),
                Duration::from_secs(30),
            )?;
        }
        let expires = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_secs()
            + 120;
        let script =
            configuration_script(&context.sid, exe, state.enabled, state.elevated, expires);
        let privileged = format!(
            "$ErrorActionPreference='Stop'\ntry {{\n{script}\nexit 0\n}} catch {{ exit 1 }}"
        );
        let path = powershell_path()?;
        let path = path.to_str().ok_or("Некорректный системный путь Windows")?;
        // Only a generated encoded command crosses the UAC boundary; no scripts
        // are loaded from a writable temporary directory. An expired UAC prompt
        // cannot apply settings long after the parent worker has timed out.
        let encoded = encoded_command(&privileged);
        if encoded.len() > 30_000 {
            return Err("Слишком длинный путь для настройки автозапуска Windows".to_owned());
        }
        let wrapper = format!(
            r#"
$process = Start-Process -FilePath {} -ArgumentList @('-NoLogo','-NoProfile','-NonInteractive','-WindowStyle','Hidden','-EncodedCommand',{}) -Verb RunAs -Wait -PassThru
if ($process.ExitCode -ne 0) {{ throw 'Startup configuration failed, was cancelled, or approval expired. Check Task Scheduler permissions and retry.' }}
"#,
            literal(path),
            literal(&encoded)
        );
        execute(&wrapper, Duration::from_secs(150))?;
        query().map(|context| context.state)
    }
    #[cfg(test)]
    mod tests {
        #[test]
        fn actual_startup_query_is_read_only_and_succeeds() {
            let context = super::query().expect("Read-only Task Scheduler query failed: Windows PowerShell and the Task Scheduler service must be available");
            assert!(context.sid.starts_with("S-1-"));
        }
    }
}

#[cfg(target_os = "windows")]
pub use windows::{Controller, State};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn powershell_literals_preserve_untrusted_path_characters() {
        assert_eq!(
            literal("C:\\Юзер's & $(evil)\\scanner.exe"),
            "'C:\\Юзер''s & $(evil)\\scanner.exe'"
        );
    }

    #[test]
    fn encoded_commands_use_utf16le_including_surrogate_pairs() {
        assert_eq!(encoded_command("A😀"), "QQA92ADe");
        assert_eq!(encoded_command(""), "");
        assert_eq!(encoded_command("A"), "QQA=");
        assert_eq!(encoded_command("AB"), "QQBCAA==");
    }

    #[test]
    fn elevated_registration_requires_protected_path_but_removal_does_not() {
        let elevated = configuration_script(
            "S-1-5-21-42",
            "C:\\Program Files\\Scanner.exe",
            true,
            true,
            12345,
        );
        let limited = configuration_script("S-1-5-21-42", "C:\\Scanner.exe", true, false, 12345);
        let removal = configuration_script("S-1-5-21-42", "C:\\Scanner.exe", false, true, 12345);
        assert!(elevated.contains("Assert-ProtectedExecutable $exe"));
        assert!(!limited.contains("Assert-ProtectedExecutable"));
        assert!(!removal.contains("Assert-ProtectedExecutable"));
        assert!(PROTECTED_PATH.contains("$chain.Reverse()"));
        assert!(PROTECTED_PATH.contains("FileAttributes]::ReparsePoint"));
        assert!(PROTECTED_PATH.contains("$null -eq $raw.DiscretionaryAcl"));
        assert!(PROTECTED_PATH.contains("$trusted -notcontains $owner"));
        assert!(PROTECTED_PATH.contains("$trusted -notcontains $rule.IdentityReference.Value"));
        assert!(!PROTECTED_PATH.contains("Set-Acl"));
    }

    #[test]
    fn registration_binds_original_sid_and_interactive_logon() {
        let script =
            configuration_script("S-1-5-21-42", "C:\\a & b\\scanner.exe", true, true, 12345);
        assert!(script.contains("$sid = 'S-1-5-21-42'"));
        assert!(script.contains("$definition.Principal.LogonType = 3"));
        assert!(script.contains("$trigger.UserId = $sid"));
        assert!(script.contains("$definition.Principal.RunLevel = 1"));
        assert!(script.contains("$expires = 12345"));
        assert!(script.contains("$flags = 2"));
        assert!(script.contains("$flags = 4"));
        assert!(!script.contains("$flags = 6"));
    }
}
