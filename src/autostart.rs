//! Per-user Windows logon task; settings are read from Task Scheduler.

#[derive(Clone, Copy, Debug, Default, serde::Deserialize, PartialEq, Eq)]
pub struct State {
    pub enabled: bool,
    pub elevated: bool,
    pub needs_repair: bool,
}

#[derive(Clone, Debug)]
pub struct Error {
    pub message: String,
    pub details: String,
    pub state: Option<State>,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<String> for Error {
    fn from(details: String) -> Self {
        let message = if details.contains("ExitCode=41") {
            "Windows не разрешила подготовить защищённую копию программы. Текущий автозапуск сохранён."
        } else if details.contains("ExitCode=42") {
            "Настройка остановлена: каталог или задача автозапуска заняты другой конфигурацией. Подробности доступны в диагностике."
        } else if details.contains("ExitCode=43") {
            "Не удалось сохранить копию программы для автозапуска. Проверьте свободное место и повторите попытку."
        } else if details.contains("ExitCode=44") {
            "Windows не разрешила сохранить задачу автозапуска. Проверьте состояние и повторите попытку."
        } else if details.contains("ExitCode=45") {
            "Файл программы изменился во время настройки. Запустите актуальную версию и повторите попытку."
        } else if details.contains("ExitCode=46") {
            "Время подтверждения Windows истекло. Повторите настройку."
        } else if details.contains("исходной учётной записи администратора")
        {
            "Для автозапуска с полным доступом войдите в учётную запись администратора Windows. Обычный автозапуск доступен без этого."
        } else if details.contains("1223")
            || details.contains("cancelled")
            || details.contains("canceled")
        {
            "Настройка отменена. Текущий автозапуск сохранён."
        } else if details.contains("expired") || details.contains("Истекло время") {
            "Не удалось дождаться подтверждения Windows. Проверьте состояние и повторите настройку."
        } else {
            "Не удалось изменить автозапуск. Повторите попытку; подробности доступны в диагностике."
        };
        Self {
            message: message.to_owned(),
            details,
            state: None,
        }
    }
}

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
function Assert-ProtectedExecutable([string]$path, [switch]$Directory) {
    $root = [System.IO.Path]::GetPathRoot($path)
    if ($root -ine [System.IO.Path]::GetPathRoot([Environment]::SystemDirectory)) {
        throw 'Elevated startup requires a protected local installation on the Windows system drive.'
    }
    $trustedInstaller = ([System.Security.Principal.NTAccount]::new('NT SERVICE\TrustedInstaller')).Translate([System.Security.Principal.SecurityIdentifier]).Value
    $trusted = @('S-1-5-18', 'S-1-5-32-544', $trustedInstaller)
    $item = Get-Item -LiteralPath $path -Force
    if ($item.PSIsContainer -ne [bool]$Directory) { throw 'Unexpected startup path type.' }
    $chain = [System.Collections.Generic.List[object]]::new()
    $chain.Add($item)
    if ($Directory) { $parent = $item.Parent } else { $parent = $item.Directory }
    while ($null -ne $parent) { $chain.Add($parent); $parent = $parent.Parent }
    $installationDirectory = if ($Directory) { $item.FullName } else { $item.Directory.FullName }
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
        # Existing directories need protection from deletion, ACL/owner and
        # attribute changes. Creating siblings cannot replace an existing chain.
        # Files additionally forbid WriteData and AppendData.
        $dangerous = 16 -bor 256 -bor 64 -bor 65536 -bor 262144 -bor 524288 -bor 0x10000000 -bor 0x40000000
        if ($entry -isnot [System.IO.DirectoryInfo] -or $entry.FullName -ieq $installationDirectory) { $dangerous = $dangerous -bor 2 -bor 4 }
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

// The destination is fixed and resolved again under the elevated token. Existing
// directories are never repaired by changing their ACLs.
const MANAGED_INSTALL: &str = r#"
function Get-ManagedExecutable([string]$userSid, [string]$digest) {
    if ($digest -notmatch '^[0-9A-Fa-f]{64}$') { throw 'Invalid startup digest.' }
    $programFiles = [Environment]::GetFolderPath([Environment+SpecialFolder]::ProgramFiles)
    if ([String]::IsNullOrEmpty($programFiles)) { throw 'Program Files known folder is unavailable.' }
    return [IO.Path]::Combine($programFiles, 'PCActivityScanner.Startup', $userSid, ('pc_activity_scanner-' + $digest.ToLowerInvariant() + '.exe'))
}
function Install-ManagedExecutable([string]$source, [string]$userSid, [string]$expectedHash) {
    $script:startupStage = 41
    $target = Get-ManagedExecutable $userSid $expectedHash
    $directory = [IO.Path]::GetDirectoryName($target)
    $productDirectory = [IO.Path]::GetDirectoryName($directory)
    $programFiles = [IO.Path]::GetDirectoryName($productDirectory)
    # Validate the complete existing chain before creating anything.
    Assert-ProtectedExecutable $programFiles -Directory
    foreach ($path in @($productDirectory, $directory)) {
        if (-not [IO.Directory]::Exists($path)) { $null = [IO.Directory]::CreateDirectory($path) }
        Assert-ProtectedExecutable $path -Directory
        $script:startupStage = 42
        $marker = [IO.Path]::Combine($path, 'owner.txt')
        $identity = 'dev.97dmitry.pc_activity_scanner.autostart.v1:'
        if ($path -ieq $directory) { $identity += $userSid } else { $identity += 'root' }
        if ([IO.File]::Exists($marker)) {
            Assert-ProtectedExecutable $marker
            if ([IO.File]::ReadAllText($marker) -cne $identity) { throw 'Managed startup directory belongs to another application.' }
        } else {
            # Never claim an existing directory containing unrelated data.
            if (@([IO.Directory]::EnumerateFileSystemEntries($path)).Count -ne 0) { throw 'Managed startup directory is not empty.' }
            $markerStream = [IO.File]::Open($marker, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
            try {
                $bytes = [Text.Encoding]::UTF8.GetBytes($identity)
                $markerStream.Write($bytes, 0, $bytes.Length)
            } finally { $markerStream.Dispose() }
            Assert-ProtectedExecutable $marker
        }
    }
    $script:startupStage = 41
    if ([IO.File]::Exists($target)) {
        Assert-ProtectedExecutable $target
        if ((Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash -ieq $expectedHash) { return $target }
        $script:startupStage = 45
        throw 'Existing managed startup file does not match its digest; refusing to overwrite.'
    }
    $staging = [IO.Path]::Combine($directory, [Guid]::NewGuid().ToString('N') + '.tmp')
    $inputStream = $null
    $outputStream = $null
    try {
        $script:startupStage = 43
        $inputStream = [IO.File]::Open($source, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
        $outputStream = [IO.File]::Open($staging, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
        $inputStream.CopyTo($outputStream)
        $outputStream.Flush($true)
        $outputStream.Dispose(); $outputStream = $null
        $script:startupStage = 45
        if ((Get-FileHash -LiteralPath $staging -Algorithm SHA256).Hash -ine $expectedHash) { throw 'Startup source changed after approval. Retry from the current application.' }
        $script:startupStage = 41
        Assert-ProtectedExecutable $staging
        if ([DateTimeOffset]::UtcNow.ToUnixTimeSeconds() -gt $expires) { $script:startupStage = 46; throw 'Startup approval expired; retry.' }
        # Digest-addressed files are never overwritten, including running images.
        $script:startupStage = 43
        [IO.File]::Move($staging, $target)
        Assert-ProtectedExecutable $target
        $script:startupStage = 45
        if ((Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash -ine $expectedHash) { throw 'Installed startup digest does not match approved executable.' }
        return $target
    } finally {
        if ($null -ne $outputStream) { $outputStream.Dispose() }
        if ($null -ne $inputStream) { $inputStream.Dispose() }
        if ([IO.File]::Exists($staging)) { [IO.File]::Delete($staging) }
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
        "$sid = {}\n$exe = {}\n$expires = {expires}\nif ([DateTimeOffset]::UtcNow.ToUnixTimeSeconds() -gt $expires) {{ $script:startupStage = 46; throw 'Startup approval expired; retry.' }}\n",
        literal(sid),
        literal(exe)
    );
    script.push_str("$script:startupStage = 42\n");
    script.push_str(TASK_CONTEXT);
    if !enabled {
        script.push_str(
            "$script:startupStage = 44\nif ($null -ne $task) { $folder.DeleteTask($name, 0) }\n",
        );
        return script;
    }
    if elevated {
        script.push_str(PROTECTED_PATH);
        script.push_str("$script:startupStage = 41\nAssert-ProtectedExecutable $exe\n");
    }
    script.push_str(&format!(r#"
$script:startupStage = 44
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
if ([DateTimeOffset]::UtcNow.ToUnixTimeSeconds() -gt $expires) {{ $script:startupStage = 46; throw 'Startup approval expired; retry.' }}
$null = $folder.RegisterTaskDefinition($name, $definition, $flags, $sid, $null, 3, $null)
"#, u8::from(elevated)));
    script
}

#[cfg(target_os = "windows")]
mod windows {
    use super::{
        Error, MANAGED_INSTALL, PROTECTED_PATH, State, TASK_CONTEXT, configuration_script,
        encoded_command, literal,
    };
    use serde::Deserialize;
    use std::io::{Read, Write};
    use std::os::windows::{ffi::OsStringExt, process::CommandExt};
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
        results: Receiver<Result<State, Error>>,
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
                        Request::Query => {
                            query()
                                .map(|context| context.state)
                                .map_err(|details| Error {
                                    message: "Не удалось проверить автозапуск. Повторите проверку."
                                        .to_owned(),
                                    details,
                                    state: None,
                                })
                        }
                        Request::Configure(state) => configure(state).map_err(|details| {
                            let mut error = Error::from(details);
                            error.state = query().ok().map(|context| context.state);
                            error
                        }),
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

        pub fn poll(&mut self) -> Option<Result<State, Error>> {
            match self.results.try_recv() {
                Ok(result) => {
                    self.busy = false;
                    Some(result)
                }
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) if self.busy => {
                    self.busy = false;
                    Some(Err(Error::from(
                        "Поток настройки автозапуска завершился".to_owned(),
                    )))
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
$ProgressPreference = 'SilentlyContinue'
$env:PSModulePath = [IO.Path]::Combine($PSHOME, 'Modules')
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
try {{
{script}
}} catch {{
    $exception = $_.Exception
    $message = [String]::Empty
    while ($null -ne $exception) {{
        $message += "HResult=" + $exception.HResult + " NativeError=" + $exception.NativeErrorCode + " " + $exception.Message + "`n"
        $exception = $exception.InnerException
    }}
    [Console]::Error.WriteLine($message)
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
    $actionPath = ''
    if ($definition.Actions.Count -eq 1 -and $definition.Actions.Item(1).Type -eq 0) {{ $actionPath = $definition.Actions.Item(1).Path }}
    $pathMatches = $actionPath -ieq $exe
    if (-not $pathMatches -and $actionPath -ieq (Get-ManagedExecutable $sid (Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash)) {{
        try {{
            Assert-ProtectedExecutable $actionPath
            $marker = [IO.Path]::Combine([IO.Path]::GetDirectoryName($actionPath), 'owner.txt')
            Assert-ProtectedExecutable $marker
            if ([IO.File]::ReadAllText($marker) -cne ('dev.97dmitry.pc_activity_scanner.autostart.v1:' + $sid)) {{ throw 'Unrecognized managed startup installation.' }}
            $pathMatches = (Get-FileHash -LiteralPath $actionPath -Algorithm SHA256).Hash -eq (Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash
        }} catch {{ $pathMatches = $false }}
    }}
    if ($definition.Principal.LogonType -ne 3 -or
        $definition.Actions.Count -ne 1 -or $definition.Triggers.Count -ne 1 -or
        $definition.Actions.Item(1).Type -ne 0 -or
        -not $pathMatches -or
        $definition.Actions.Item(1).WorkingDirectory -ine [IO.Path]::GetDirectoryName($actionPath) -or
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
            format_args!("{}\n{}", PROTECTED_PATH, MANAGED_INSTALL)
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
        let expires = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_secs()
            + 120;
        let mut script =
            configuration_script(&context.sid, exe, state.enabled, state.elevated, expires);
        if state.enabled && state.elevated {
            let hash = execute(
                &format!(
                    "(Get-FileHash -LiteralPath {} -Algorithm SHA256).Hash",
                    literal(exe)
                ),
                Duration::from_secs(30),
            )?;
            let hash = hash.trim();
            if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("Invalid executable SHA256".to_owned());
            }
            // The source and its digest are fixed before approval. The elevated
            // helper reads the source through a handle that denies concurrent writes.
            let prepare = format!(
                "$expectedHash = {}\n{}\n$exe = Install-ManagedExecutable $exe $sid $expectedHash\n",
                literal(hash),
                MANAGED_INSTALL
            );
            let marker = "Assert-ProtectedExecutable $exe\n";
            let offset = script
                .find(marker)
                .ok_or("Missing protected installation guard")?;
            script.insert_str(offset, &prepare);
        }
        // Ordinary per-user startup needs no consent for a new/limited task.
        // Replacing or deleting a task formerly registered as Highest can require UAC.
        if !state.enabled || !state.elevated {
            match execute(&script, Duration::from_secs(30)) {
                Ok(_) => return query().map(|context| context.state),
                Err(error)
                    if !error.contains("-2147024891")
                        && !error.contains("80070005")
                        && !error.contains("Access is denied")
                        && !error.contains("E_ACCESSDENIED") =>
                {
                    return Err(error);
                }
                Err(_) => {}
            }
        }
        let privileged = format!(
            "$ErrorActionPreference='Stop'\n$ProgressPreference='SilentlyContinue'\n$env:PSModulePath=[IO.Path]::Combine($PSHOME,'Modules')\n$script:startupStage=44\ntry {{\n{script}\nexit 0\n}} catch {{ exit $script:startupStage }}"
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
$process = Start-Process -FilePath {} -ArgumentList @('-NoLogo','-NoProfile','-NonInteractive','-WindowStyle','Hidden','-EncodedCommand',{}) -WorkingDirectory ([Environment]::SystemDirectory) -Verb RunAs -Wait -PassThru
if ($process.ExitCode -ne 0) {{ throw ('Startup helper failed. ExitCode=' + $process.ExitCode + '; stages: 41=protected path, 42=ownership, 43=copy, 44=task registration, 45=SHA256, 46=approval expiry. Child stderr is unavailable across UAC.') }}
"#,
            literal(path),
            literal(&encoded)
        );
        execute(&wrapper, Duration::from_secs(150))?;
        query().map(|context| context.state)
    }
    #[cfg(test)]
    mod tests {
        /// Explicitly invoked only in a disposable Windows CI runner. Never uses
        /// the production task name and never enables Highest or installs files.
        #[test]
        #[ignore = "Mutates only a unique disposable CI Task Scheduler task; requires explicit opt-in"]
        fn disposable_ci_limited_task_roundtrip() {
            assert_eq!(std::env::var("GITHUB_ACTIONS").as_deref(), Ok("true"));
            assert_eq!(
                std::env::var("PC_ACTIVITY_SCANNER_CI_TASK_TEST").as_deref(),
                Ok("1")
            );
            let identity = super::query().expect("Read original identity");
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let task_name = format!("PCActivityScanner-CI-{}-{nonce}", std::process::id());
            let exe = std::env::current_exe()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned();
            let expires = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                + 120;
            let original = "$name = 'PCActivityScanner-' + $sid";
            let replacement = format!("$name = {}", super::literal(&task_name));
            let script = super::configuration_script(&identity.sid, &exe, true, false, expires)
                .replace(original, &replacement);
            let cleanup = super::configuration_script(&identity.sid, &exe, false, false, expires)
                .replace(original, &replacement);
            let result = super::execute(&script, std::time::Duration::from_secs(30));
            // Cleanup is attempted even if registration returns an error.
            let verified = if result.is_ok() {
                let inspect = format!(
                    "$sid = {}\n{}\nif ($null -eq $task -or -not $task.Enabled -or $task.Definition.Principal.RunLevel -ne 0 -or $task.Definition.Actions.Item(1).Path -ine {}) {{ throw 'CI task configuration differs from request.' }}",
                    super::literal(&identity.sid),
                    super::TASK_CONTEXT.replace(original, &replacement),
                    super::literal(&exe)
                );
                super::execute(&inspect, std::time::Duration::from_secs(30))
            } else {
                Ok(String::new())
            };
            let removed = super::execute(&cleanup, std::time::Duration::from_secs(30));
            result.expect("Limited per-user registration failed");
            verified.expect("Read-back task validation failed");
            removed.expect("CI task cleanup failed");
            let absent = format!(
                "$sid = {}\n{}\nif ($null -ne $task) {{ throw 'CI task was not removed.' }}",
                super::literal(&identity.sid),
                super::TASK_CONTEXT.replace(original, &replacement)
            );
            super::execute(&absent, std::time::Duration::from_secs(30)).expect("Verify removal");
        }

        #[test]
        #[ignore = "Installs and removes a uniquely named protected scratch copy only on an opted-in disposable Windows CI runner"]
        fn disposable_ci_managed_install_roundtrip() {
            assert_eq!(std::env::var("GITHUB_ACTIONS").as_deref(), Ok("true"));
            assert_eq!(
                std::env::var("PC_ACTIVITY_SCANNER_CI_TASK_TEST").as_deref(),
                Ok("1")
            );
            let identity = super::query().expect("Read original identity");
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let product = format!("PCActivityScanner.CI-{}-{nonce}", std::process::id());
            let exe = std::env::current_exe()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned();
            let installer = super::MANAGED_INSTALL
                .replace("'PCActivityScanner.Startup'", &super::literal(&product));
            let task_name = format!(
                "PCActivityScanner-CI-Highest-{}-{nonce}",
                std::process::id()
            );
            let original = "$name = 'PCActivityScanner-' + $sid";
            let replacement = format!("$name = {}", super::literal(&task_name));
            let task_context = super::TASK_CONTEXT.replace(original, &replacement);
            let registration =
                super::configuration_script(&identity.sid, "unused", true, true, 4_000_000_000)
                    .replace(original, &replacement)
                    .replace("$exe = 'unused'", "$exe = $target");
            let cleanup_task =
                super::configuration_script(&identity.sid, "unused", false, false, 4_000_000_000)
                    .replace(original, &replacement);
            let script = format!(
                r#"
{}
{}
$sid = {}
$source = {}
$expires = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds() + 120
$hash = (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash
$target = Get-ManagedExecutable $sid $hash
$directory = [IO.Path]::GetDirectoryName($target)
$productDirectory = [IO.Path]::GetDirectoryName($directory)
try {{
    $installed = Install-ManagedExecutable $source $sid $hash
    if ($installed -ine $target) {{ throw 'Installer returned unexpected path.' }}
    if ((Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash -ine $hash) {{ throw 'Installed digest mismatch.' }}
    {}
    {}
    if ($null -eq $task -or -not $task.Enabled -or $task.Definition.Principal.RunLevel -ne 1 -or $task.Definition.Actions.Item(1).Path -ine $target) {{ throw 'Highest task configuration differs from installed copy.' }}
    $second = Install-ManagedExecutable $target $sid $hash
    if ($second -ine $target) {{ throw 'Repeated installation failed.' }}
    $rejected = $false
    try {{ $null = Install-ManagedExecutable $source $sid ('0' * 64) }} catch {{ $rejected = $true }}
    if (-not $rejected) {{ throw 'Changed source digest was accepted.' }}
    if ((Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash -ine $hash) {{ throw 'Failed installation replaced original copy.' }}
}} finally {{
    {}
    # No recursive deletion: only exact files produced by this unique test.
    foreach ($file in @($target, [IO.Path]::Combine($directory, 'owner.txt'), [IO.Path]::Combine($productDirectory, 'owner.txt'))) {{
        if ([IO.File]::Exists($file)) {{ Assert-ProtectedExecutable $file; [IO.File]::Delete($file) }}
    }}
    foreach ($path in @($directory, $productDirectory)) {{
        if ([IO.Directory]::Exists($path)) {{ Assert-ProtectedExecutable $path -Directory; [IO.Directory]::Delete($path, $false) }}
    }}
}}
"#,
                super::PROTECTED_PATH,
                installer,
                super::literal(&identity.sid),
                super::literal(&exe),
                registration,
                task_context,
                cleanup_task
            );
            super::execute(&script, std::time::Duration::from_secs(150))
                .expect("Disposable protected installation roundtrip failed");
        }

        #[test]
        fn actual_startup_query_is_read_only_and_succeeds() {
            let context = super::query().expect("Read-only Task Scheduler query failed: Windows PowerShell and the Task Scheduler service must be available");
            assert!(context.sid.starts_with("S-1-"));
        }
    }
}

#[cfg(target_os = "windows")]
pub use windows::Controller;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_keep_shell_diagnostics_out_of_product_messages() {
        let details = "#< CLIXML <Objs>Elevated startup refuses user-writable paths</Objs>";
        let error = Error::from(details.to_owned());
        assert!(!error.message.contains("CLIXML"));
        assert!(!error.message.contains("user-writable"));
        assert_eq!(error.details, details);
        assert!(error.state.is_none());
        assert!(
            Error::from("Startup helper failed. ExitCode=45".to_owned())
                .message
                .contains("изменился")
        );
        assert!(
            Error::from("Startup helper failed. ExitCode=42".to_owned())
                .message
                .contains("другой конфигурацией")
        );
        assert_eq!(error.to_string(), error.message);
        assert!(
            Error::from("NativeError=1223".to_owned())
                .message
                .contains("отменена")
        );
    }

    #[test]
    fn managed_install_pins_copy_and_validates_before_mutations() {
        let guard = MANAGED_INSTALL
            .find("Assert-ProtectedExecutable $programFiles -Directory")
            .unwrap();
        let create = MANAGED_INSTALL
            .find("[IO.Directory]::CreateDirectory")
            .unwrap();
        let copy = MANAGED_INSTALL
            .find("$inputStream.CopyTo($outputStream)")
            .unwrap();
        let hash = MANAGED_INSTALL.find(".Hash -ine $expectedHash").unwrap();
        let replace = MANAGED_INSTALL.find("[IO.File]::Move").unwrap();
        assert!(guard < create && copy < hash && hash < replace);
        assert!(MANAGED_INSTALL.contains("[IO.FileShare]::Read)"));
        assert!(MANAGED_INSTALL.contains("[IO.FileMode]::CreateNew"));
        assert!(MANAGED_INSTALL.contains("SpecialFolder]::ProgramFiles"));
        assert!(!MANAGED_INSTALL.contains("$env:"));
        assert!(!MANAGED_INSTALL.contains("Set-Acl"));
        assert!(MANAGED_INSTALL.contains("ReadAllText($marker) -cne $identity"));
        assert!(MANAGED_INSTALL.contains("$digest.ToLowerInvariant()"));
        assert!(!MANAGED_INSTALL.contains("[IO.File]::Replace"));
    }

    #[test]
    fn elevated_payload_fits_windows_command_line() {
        let mut script = configuration_script(
            "S-1-5-21-123-456-789-1000",
            "C:\\Users\\test\\Downloads\\pc_activity_scanner.exe",
            true,
            true,
            12345,
        );
        script.push_str(MANAGED_INSTALL);
        script.push_str(&"a".repeat(256));
        assert!(
            encoded_command(&script).len() < 30_000,
            "UAC payload exceeds Windows command line limit"
        );
    }

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
