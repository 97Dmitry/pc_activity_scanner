//! Selected-process endpoint inspection. No DNS, payloads, or per-connection traffic estimates.
use crate::model::ProcessIdentity;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_ROWS: usize = 4096;
#[cfg(target_os = "linux")]
const MAX_FDS: usize = 65_536;
const MAX_TABLE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Connection {
    pub protocol: String,
    pub local: String,
    pub remote: Option<String>,
    pub state: Option<String>,
}
#[derive(Clone, Debug, Default)]
pub struct ConnectionView {
    pub identity: Option<ProcessIdentity>,
    pub rows: Vec<Connection>,
    pub status: String,
    pub updated_at: Option<Instant>,
}
#[derive(Default)]
struct State {
    generation: u64,
    selected: Option<ProcessIdentity>,
    view: Arc<ConnectionView>,
}
impl State {
    fn select(&mut self, identity: Option<ProcessIdentity>) {
        if self.selected == identity {
            return;
        }
        self.generation = self.generation.wrapping_add(1);
        self.selected = identity;
        self.view = Arc::new(ConnectionView {
            identity,
            status: if identity.is_some() {
                "Обновление соединений…".into()
            } else {
                "Выберите отдельный процесс".into()
            },
            ..Default::default()
        });
    }
    fn publish(&mut self, generation: u64, view: ConnectionView) -> bool {
        if self.generation != generation || self.selected != view.identity {
            return false;
        }
        self.view = Arc::new(view);
        true
    }
}
pub struct Connections {
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
}
impl Default for Connections {
    fn default() -> Self {
        let state = Arc::new(Mutex::new(State::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_state = Arc::clone(&state);
        let worker_stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut last: Option<(u64, Instant)> = None;
            while !worker_stop.load(Ordering::Relaxed) {
                let (generation, identity) = {
                    let state = worker_state
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    (state.generation, state.selected)
                };
                if let Some(identity) = identity
                    && last.is_none_or(|(old, at)| {
                        old != generation || at.elapsed() >= Duration::from_secs(2)
                    })
                {
                    last = Some((generation, Instant::now()));
                    let view = query(identity);
                    worker_state
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .publish(generation, view);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        Self { state, stop }
    }
}
impl Connections {
    pub fn select(&self, identity: Option<ProcessIdentity>) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .select(identity);
    }
    pub fn view(&self) -> Arc<ConnectionView> {
        Arc::clone(
            &self
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .view,
        )
    }
}
impl Drop for Connections {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
#[derive(Default)]
struct SourceResult {
    rows: Vec<Connection>,
    issues: Vec<String>,
    truncated: bool,
}
pub(crate) fn query(identity: ProcessIdentity) -> ConnectionView {
    let mut view = ConnectionView {
        identity: Some(identity),
        updated_at: Some(Instant::now()),
        ..Default::default()
    };
    if identity.lifetime.is_none()
        || crate::process_details::process_lifetime(identity.pid) != identity.lifetime
    {
        view.status="Выбранное время жизни процесса недоступно: процесс завершён или идентичность не подтверждена".into();
        return view;
    }
    let source = platform::read(identity.pid);
    if crate::process_details::process_lifetime(identity.pid) != identity.lifetime {
        view.status = "Процесс завершился или изменился во время чтения соединений".into();
        return view;
    }
    view.updated_at = Some(Instant::now());
    match source {
        Ok(mut source) => {
            source.rows.sort_by(|a, b| {
                (&a.protocol, &a.local, &a.remote, &a.state).cmp(&(
                    &b.protocol,
                    &b.local,
                    &b.remote,
                    &b.state,
                ))
            });
            source.rows.dedup();
            let limit = if source.truncated {
                "; достигнут лимит, список неполный"
            } else {
                ""
            };
            let issues = if source.issues.is_empty() {
                String::new()
            } else {
                format!("; часть данных недоступна: {}", source.issues.join("; "))
            };
            view.status = format!(
                "Доступных конечных точек: {} ({}){limit}{issues}",
                source.rows.len(),
                platform::SOURCE
            );
            view.rows = source.rows;
        }
        Err(error) => view.status = format!("Соединения процесса недоступны: {error}"),
    }
    view
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use std::collections::HashSet;
    pub const SOURCE: &str = "Linux procfs, открытые socket-дескрипторы";
    fn address(text: &str, ipv6: bool) -> Option<String> {
        use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};
        let (ip, port) = text.split_once(':')?;
        if port.len() != 4 {
            return None;
        }
        let port = u16::from_str_radix(port, 16).ok()?;
        if ipv6 {
            if ip.len() != 32 || !ip.is_ascii() {
                return None;
            }
            let mut bytes = [0u8; 16];
            for (index, part) in ip.as_bytes().chunks_exact(8).enumerate() {
                let value = u32::from_str_radix(std::str::from_utf8(part).ok()?, 16).ok()?;
                bytes[index * 4..index * 4 + 4].copy_from_slice(&value.to_ne_bytes());
            }
            Some(SocketAddrV6::new(Ipv6Addr::from(bytes), port, 0, 0).to_string())
        } else {
            if ip.len() != 8 {
                return None;
            }
            let value = u32::from_str_radix(ip, 16).ok()?;
            Some(SocketAddrV4::new(Ipv4Addr::from(value.to_ne_bytes()), port).to_string())
        }
    }
    fn state(code: u8) -> String {
        match code {
            1 => "ESTABLISHED",
            2 => "SYN_SENT",
            3 => "SYN_RECV",
            4 => "FIN_WAIT1",
            5 => "FIN_WAIT2",
            6 => "TIME_WAIT",
            7 => "CLOSED",
            8 => "CLOSE_WAIT",
            9 => "LAST_ACK",
            10 => "LISTEN",
            11 => "CLOSING",
            12 => "NEW_SYN_RECV",
            _ => return format!("UNKNOWN(0x{code:02X})"),
        }
        .into()
    }
    fn parse_table(
        text: &str,
        tcp: bool,
        ipv6: bool,
        owned: &HashSet<u64>,
    ) -> Result<(Vec<(u64, Connection)>, bool), String> {
        if text.len() > MAX_TABLE_BYTES {
            return Err("Socket table exceeds 8-MiB limit".into());
        }
        let mut result = Vec::new();
        for line in text.lines().skip(1) {
            let fields: Vec<_> = line.split_whitespace().take(10).collect();
            if fields.len() < 10 {
                return Err("Malformed procfs socket table row".into());
            }
            let inode = fields[9]
                .parse::<u64>()
                .map_err(|_| "Invalid socket inode")?;
            if !owned.contains(&inode) {
                continue;
            }
            let local = address(fields[1], ipv6).ok_or("Invalid local socket address")?;
            let remote = address(fields[2], ipv6).ok_or("Invalid remote socket address")?;
            let code = u8::from_str_radix(fields[3], 16).map_err(|_| "Invalid socket state")?;
            if result.len() == MAX_ROWS {
                return Ok((result, true));
            }
            let remote = (code != 10 || !tcp).then_some(remote).filter(|remote| {
                remote
                    .parse::<std::net::SocketAddr>()
                    .is_ok_and(|address| address.port() != 0 || !address.ip().is_unspecified())
            });
            result.push((
                inode,
                Connection {
                    protocol: format!(
                        "{}/IPv{}",
                        if tcp { "TCP" } else { "UDP" },
                        if ipv6 { 6 } else { 4 }
                    ),
                    local,
                    remote,
                    state: tcp.then(|| state(code)),
                },
            ));
        }
        Ok((result, false))
    }
    struct Inodes {
        owned: HashSet<u64>,
        partial: bool,
    }
    fn socket_inodes(pid: u32) -> Result<Inodes, String> {
        let path = format!("/proc/{pid}/fd");
        let entries = std::fs::read_dir(&path).map_err(|error| format!("{path}: {error}"))?;
        let mut owned = HashSet::new();
        let mut partial = false;
        for (index, entry) in entries.enumerate() {
            if index == MAX_FDS {
                partial = true;
                break;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    partial = true;
                    continue;
                }
            };
            match std::fs::read_link(entry.path()) {
                Ok(target) => {
                    if let Some(inode) = target
                        .to_str()
                        .and_then(|target| target.strip_prefix("socket:["))
                        .and_then(|value| value.strip_suffix(']'))
                        .and_then(|value| value.parse::<u64>().ok())
                    {
                        owned.insert(inode);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => partial = true,
            }
        }
        Ok(Inodes { owned, partial })
    }
    fn text(path: &str) -> Result<String, String> {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .map_err(|error| format!("{path}: {error}"))?
            .take(MAX_TABLE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        if bytes.len() > MAX_TABLE_BYTES {
            return Err(format!("{path}: socket table exceeds 8-MiB limit"));
        }
        String::from_utf8(bytes).map_err(|error| error.to_string())
    }
    pub(super) fn read(pid: u32) -> Result<SourceResult, String> {
        let before = socket_inodes(pid)?;
        let mut source = SourceResult::default();
        let mut pairs = Vec::new();
        for (name, tcp, ipv6) in [
            ("tcp", true, false),
            ("tcp6", true, true),
            ("udp", false, false),
            ("udp6", false, true),
        ] {
            let path = format!("/proc/{pid}/net/{name}");
            match text(&path).and_then(|text| parse_table(&text, tcp, ipv6, &before.owned)) {
                Ok((rows, truncated)) => {
                    source.truncated |= truncated || pairs.len() + rows.len() > MAX_ROWS;
                    pairs.extend(rows.into_iter().take(MAX_ROWS - pairs.len()));
                }
                Err(error) => source.issues.push(error),
            }
        }
        let after = socket_inodes(pid)?;
        if before.partial || after.partial {
            source
                .issues
                .push("Socket descriptor scan incomplete or reached 65536-fd limit".into());
        }
        source.rows = pairs
            .into_iter()
            .filter(|(inode, _)| after.owned.contains(inode))
            .map(|(_, connection)| connection)
            .collect();
        Ok(source)
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn proc_addresses_decode_native_endian_words_and_hex_ports() {
            assert_eq!(
                address("0100007F:1F90", false).as_deref(),
                Some("127.0.0.1:8080")
            );
            assert_eq!(
                address("00000000000000000000000001000000:01BB", true).as_deref(),
                Some("[::1]:443")
            );
            assert_eq!(
                address("000080FE000000000000000001000000:0035", true).as_deref(),
                Some("[fe80::1]:53")
            );
            assert!(address("0100007F:10000", false).is_none());
            assert!(address("invalid:0035", true).is_none());
        }
        #[test]
        fn proc_rows_require_owned_inode_and_keep_listen_and_udp_remote_semantics() {
            let table = "sl local_address rem_address st tx_queue rx_queue tr tm->when retrnsmt uid timeout inode\n 0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000 1000 0 123 1\n 1: 0100007F:CAFE 0100007F:1F90 01 00000000:00000000 00:00000000 00000000 1000 0 124 1\n";
            let owned = [123].into();
            let (rows, truncated) = parse_table(table, true, false, &owned).unwrap();
            assert!(!truncated);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].1.local, "127.0.0.1:8080");
            assert_eq!(rows[0].1.remote, None);
            assert_eq!(rows[0].1.state.as_deref(), Some("LISTEN"));
            let (udp, _) = parse_table(table, false, false, &owned).unwrap();
            assert_eq!(udp[0].1.state, None);
            assert_eq!(udp[0].1.remote, None);
        }
        #[test]
        fn proc_table_and_output_limits_are_explicit() {
            assert!(
                parse_table(
                    &"x".repeat(MAX_TABLE_BYTES + 1),
                    true,
                    false,
                    &HashSet::new()
                )
                .is_err()
            );
            let entry = "0: 0100007F:1F90 00000000:0000 0A 0:0 0:0 0 1000 0 123\n";
            let table = format!("header\n{}", entry.repeat(MAX_ROWS + 1));
            let (rows, truncated) = parse_table(&table, true, false, &[123].into()).unwrap();
            assert_eq!(rows.len(), MAX_ROWS);
            assert!(truncated);
        }
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};
    use windows_sys::Win32::NetworkManagement::IpHelper::*;
    use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};
    pub const SOURCE: &str = "Windows IP Helper owner-PID tables";
    fn ipv4(address: u32, port: u32) -> String {
        SocketAddrV4::new(
            Ipv4Addr::from(address.to_ne_bytes()),
            u16::from_be(port as u16),
        )
        .to_string()
    }
    fn ipv6(address: [u8; 16], scope: u32, port: u32) -> String {
        SocketAddrV6::new(
            Ipv6Addr::from(address),
            u16::from_be(port as u16),
            0,
            u32::from_be(scope),
        )
        .to_string()
    }
    fn state(code: u32) -> String {
        match code {
            1 => "CLOSED",
            2 => "LISTEN",
            3 => "SYN_SENT",
            4 => "SYN_RECV",
            5 => "ESTABLISHED",
            6 => "FIN_WAIT1",
            7 => "FIN_WAIT2",
            8 => "CLOSE_WAIT",
            9 => "CLOSING",
            10 => "LAST_ACK",
            11 => "TIME_WAIT",
            12 => "DELETE_TCB",
            _ => return format!("UNKNOWN({code})"),
        }
        .into()
    }
    fn table<T: Copy>(tcp: bool, ipv6: bool, offset: usize) -> Result<Vec<T>, String> {
        let api = if tcp {
            GetExtendedTcpTable
        } else {
            GetExtendedUdpTable
        };
        let class = if tcp {
            TCP_TABLE_OWNER_PID_ALL
        } else {
            UDP_TABLE_OWNER_PID
        };
        let family = if ipv6 { AF_INET6 } else { AF_INET } as u32;
        let mut bytes = 0;
        let status = unsafe { api(std::ptr::null_mut(), &mut bytes, 0, family, class, 0) };
        if status != 122 && status != 0 {
            return Err(format!("IP Helper table size: Win32 {status}"));
        }
        for _ in 0..3 {
            if bytes as usize > MAX_TABLE_BYTES || (bytes as usize) < offset {
                return Err("IP Helper table exceeds bounds".into());
            }
            let mut allocation = vec![0u64; (bytes as usize).div_ceil(8)];
            let capacity = allocation.len() * 8;
            bytes = capacity as u32;
            let status = unsafe {
                api(
                    allocation.as_mut_ptr().cast(),
                    &mut bytes,
                    0,
                    family,
                    class,
                    0,
                )
            };
            if status == 122 {
                continue;
            }
            if status != 0 {
                return Err(format!("IP Helper table read: Win32 {status}"));
            }
            if (bytes as usize) < offset || bytes as usize > capacity {
                return Err("Invalid IP Helper table size".into());
            }
            let data = allocation.as_ptr().cast::<u8>();
            let count = unsafe { data.cast::<u32>().read_unaligned() } as usize;
            if count > (bytes as usize - offset) / std::mem::size_of::<T>() {
                return Err("IP Helper row count exceeds buffer".into());
            }
            let mut rows = Vec::with_capacity(count);
            for index in 0..count {
                rows.push(unsafe {
                    data.add(offset + index * std::mem::size_of::<T>())
                        .cast::<T>()
                        .read_unaligned()
                });
            }
            return Ok(rows);
        }
        Err("IP Helper table changed repeatedly while reading".into())
    }
    fn tcp4(pid: u32) -> Result<Vec<Connection>, String> {
        let rows = table::<MIB_TCPROW_OWNER_PID>(
            true,
            false,
            std::mem::offset_of!(MIB_TCPTABLE_OWNER_PID, table),
        )?;
        Ok(rows
            .into_iter()
            .filter(|row| row.dwOwningPid == pid)
            .take(MAX_ROWS + 1)
            .map(|row| Connection {
                protocol: "TCP/IPv4".into(),
                local: ipv4(row.dwLocalAddr, row.dwLocalPort),
                remote: (row.dwState != 2).then(|| ipv4(row.dwRemoteAddr, row.dwRemotePort)),
                state: Some(state(row.dwState)),
            })
            .collect())
    }
    fn tcp6(pid: u32) -> Result<Vec<Connection>, String> {
        let rows = table::<MIB_TCP6ROW_OWNER_PID>(
            true,
            true,
            std::mem::offset_of!(MIB_TCP6TABLE_OWNER_PID, table),
        )?;
        Ok(rows
            .into_iter()
            .filter(|row| row.dwOwningPid == pid)
            .take(MAX_ROWS + 1)
            .map(|row| Connection {
                protocol: "TCP/IPv6".into(),
                local: ipv6(row.ucLocalAddr, row.dwLocalScopeId, row.dwLocalPort),
                remote: (row.dwState != 2)
                    .then(|| ipv6(row.ucRemoteAddr, row.dwRemoteScopeId, row.dwRemotePort)),
                state: Some(state(row.dwState)),
            })
            .collect())
    }
    fn udp4(pid: u32) -> Result<Vec<Connection>, String> {
        let rows = table::<MIB_UDPROW_OWNER_PID>(
            false,
            false,
            std::mem::offset_of!(MIB_UDPTABLE_OWNER_PID, table),
        )?;
        Ok(rows
            .into_iter()
            .filter(|row| row.dwOwningPid == pid)
            .take(MAX_ROWS + 1)
            .map(|row| Connection {
                protocol: "UDP/IPv4".into(),
                local: ipv4(row.dwLocalAddr, row.dwLocalPort),
                remote: None,
                state: None,
            })
            .collect())
    }
    fn udp6(pid: u32) -> Result<Vec<Connection>, String> {
        let rows = table::<MIB_UDP6ROW_OWNER_PID>(
            false,
            true,
            std::mem::offset_of!(MIB_UDP6TABLE_OWNER_PID, table),
        )?;
        Ok(rows
            .into_iter()
            .filter(|row| row.dwOwningPid == pid)
            .take(MAX_ROWS + 1)
            .map(|row| Connection {
                protocol: "UDP/IPv6".into(),
                local: ipv6(row.ucLocalAddr, row.dwLocalScopeId, row.dwLocalPort),
                remote: None,
                state: None,
            })
            .collect())
    }
    pub(super) fn read(pid: u32) -> Result<SourceResult, String> {
        let mut source = SourceResult::default();
        for (name, result) in [
            ("TCP/IPv4", tcp4(pid)),
            ("TCP/IPv6", tcp6(pid)),
            ("UDP/IPv4", udp4(pid)),
            ("UDP/IPv6", udp6(pid)),
        ] {
            match result {
                Ok(rows) => {
                    source.truncated |= source.rows.len() + rows.len() > MAX_ROWS;
                    source
                        .rows
                        .extend(rows.into_iter().take(MAX_ROWS - source.rows.len()));
                }
                Err(error) => source.issues.push(format!("{name}: {error}")),
            }
        }
        Ok(source)
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn windows_network_byte_order_and_ipv6_scopes_are_preserved() {
            assert_eq!(
                ipv4(u32::from_ne_bytes([127, 0, 0, 1]), u16::to_be(8080) as u32),
                "127.0.0.1:8080"
            );
            let address = Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1).octets();
            assert_eq!(
                ipv6(address, u32::to_be(3), u16::to_be(443) as u32),
                "[fe80::1%3]:443"
            );
            assert_eq!(state(5), "ESTABLISHED");
            assert_eq!(state(2), "LISTEN");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn identity(token: u64) -> ProcessIdentity {
        ProcessIdentity {
            pid: 42,
            started_at: 1,
            lifetime: Some(token),
        }
    }
    #[test]
    fn native_own_loopback_endpoints_cover_tcp_udp_ipv4_and_supported_ipv6() {
        use std::net::{TcpListener, TcpStream, UdpSocket};
        let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
        let tcp_address = tcp.local_addr().unwrap();
        let client = TcpStream::connect_timeout(&tcp_address, Duration::from_secs(1)).unwrap();
        tcp.set_nonblocking(true).unwrap();
        let accept_deadline = Instant::now() + Duration::from_secs(1);
        let server = loop {
            match tcp.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < accept_deadline =>
                {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(error) => panic!("Loopback TCP accept failed: {error}"),
            }
        };
        let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        let udp_address = udp.local_addr().unwrap();
        let udp_peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        udp.connect(udp_peer.local_addr().unwrap()).unwrap();
        let ipv6 = match TcpListener::bind("[::1]:0") {
            Ok(listener) => Some((listener, UdpSocket::bind("[::1]:0").unwrap())),
            Err(error)
                if error.kind() == std::io::ErrorKind::AddrNotAvailable
                    || error.kind() == std::io::ErrorKind::Unsupported
                    || [Some(97), Some(10047)].contains(&error.raw_os_error()) =>
            {
                eprintln!("IPv6 loopback unsupported: {error}");
                None
            }
            Err(error) => panic!("IPv6 loopback bind failed: {error}"),
        };
        let pid = std::process::id();
        let mut process_rows = vec![crate::model::ProcessRow {
            pid,
            ..Default::default()
        }];
        crate::process_details::ProcessDetails::default().sample(&mut process_rows, 1, 0.0);
        let identity = process_rows
            .iter()
            .find(|row| row.pid == pid)
            .unwrap()
            .identity();
        let view = query(identity);
        assert!(view.status.starts_with("Доступных"), "{}", view.status);
        assert!(!view.status.contains("недоступна"), "{}", view.status);
        assert!(
            view.rows.iter().any(|row| row.protocol == "TCP/IPv4"
                && row.local == tcp_address.to_string()
                && row.state.as_deref() == Some("LISTEN")
                && row.remote.is_none()),
            "TCP4 listener missing: {:?}",
            view
        );
        assert!(
            view.rows.iter().any(|row| row.protocol == "TCP/IPv4"
                && row.state.as_deref() == Some("ESTABLISHED")
                && row.remote.as_deref() == Some(tcp_address.to_string().as_str())),
            "TCP4 established missing: {:?}",
            view
        );
        assert!(
            view.rows.iter().any(|row| row.protocol == "UDP/IPv4"
                && row.local == udp_address.to_string()
                && row.state.is_none()),
            "UDP4 missing: {:?}",
            view
        );
        #[cfg(target_os = "linux")]
        assert!(
            view.rows
                .iter()
                .any(|row| row.local == udp_address.to_string()
                    && row.remote.as_deref()
                        == Some(udp_peer.local_addr().unwrap().to_string().as_str())),
            "connected UDP remote missing: {:?}",
            view
        );
        #[cfg(target_os = "windows")]
        assert!(
            view.rows
                .iter()
                .filter(|row| row.protocol.starts_with("UDP"))
                .all(|row| row.remote.is_none())
        );
        if let Some((tcp6, udp6)) = &ipv6 {
            assert!(
                view.rows.iter().any(|row| row.protocol == "TCP/IPv6"
                    && row.local == tcp6.local_addr().unwrap().to_string()),
                "TCP6 missing: {:?}",
                view
            );
            assert!(
                view.rows.iter().any(|row| row.protocol == "UDP/IPv6"
                    && row.local == udp6.local_addr().unwrap().to_string()),
                "UDP6 missing: {:?}",
                view
            );
        }
        eprintln!(
            "Native selected-process endpoints: {}; IPv6={}; TCP4 listen+established and UDP4 confirmed",
            view.status,
            ipv6.is_some()
        );
        let service = Connections::default();
        service.select(Some(identity));
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let current = service.view();
            assert_eq!(current.identity, Some(identity));
            if current.updated_at.is_some() {
                assert!(!current.rows.is_empty(), "{}", current.status);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Background endpoint query timed out"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        service.select(None);
        assert_eq!(service.view().identity, None);
        assert!(service.view().rows.is_empty());
        drop(service);
        drop((client, server, tcp, udp, udp_peer, ipv6));
        let closed = query(identity);
        assert!(!closed.rows.iter().any(|row| row.protocol == "TCP/IPv4"
            && row.state.as_deref() == Some("LISTEN")
            && row.local == tcp_address.to_string()));
        assert!(
            !closed
                .rows
                .iter()
                .any(|row| row.protocol == "UDP/IPv4" && row.local == udp_address.to_string())
        );
    }
    #[test]
    fn selection_changes_clear_rows_and_reject_slow_old_publication() {
        let mut state = State::default();
        state.select(Some(identity(100)));
        let old = state.generation;
        let oldview = ConnectionView {
            identity: Some(identity(100)),
            status: "old".into(),
            ..Default::default()
        };
        assert!(state.publish(old, oldview.clone()));
        state.select(Some(identity(200)));
        assert!(state.view.rows.is_empty());
        assert_eq!(state.view.identity, Some(identity(200)));
        assert!(!state.publish(old, oldview));
        assert_eq!(state.view.identity, Some(identity(200)));
        let current = state.generation;
        state.select(Some(identity(200)));
        assert_eq!(state.generation, current);
        state.select(None);
        assert_eq!(state.view.identity, None);
    }
    #[test]
    fn missing_or_reused_lifetime_never_queries_another_process() {
        let mut id = identity(100);
        id.pid = std::process::id();
        let actual = crate::process_details::process_lifetime(id.pid).unwrap();
        id.lifetime = Some(actual.wrapping_add(1));
        let result = query(id);
        assert!(result.rows.is_empty());
        assert!(result.status.contains("процесс") || result.status.contains("Процесс"));
        id.lifetime = None;
        let result = query(id);
        assert!(result.rows.is_empty());
        assert!(!result.status.is_empty());
    }
}
