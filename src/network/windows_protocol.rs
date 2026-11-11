//! Bounded helper protocol and the documented common prefix of kernel IP events.
const MAGIC: &[u8; 8] = b"PCATW001";
pub const MAX_PROCESSES: usize = 4096;
pub const MAX_MESSAGE: usize = 40 + MAX_PROCESSES * 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Traffic {
    pub pid: u32,
    pub created: u64,
    pub uploaded: u64,
    pub downloaded: u64,
}

#[derive(Debug)]
pub struct Frame {
    pub elapsed_ns: u64,
    pub captured_at: u64,
    pub lost: u64,
    pub processes: Vec<Traffic>,
}

/// Version 2 TCP/UDP IPv4 and IPv6 send/receive share PID,size as their first fields.
/// Deliberately exclude retransmissions and version 0/1 layouts.
pub fn parse_ip_event(version: u8, opcode: u8, data: &[u8]) -> Option<(u32, u32, bool)> {
    if version != 2 || !matches!(opcode, 10 | 11 | 26 | 27) || data.len() < 8 {
        return None;
    }
    let pid = u32::from_le_bytes(data[..4].try_into().ok()?);
    let size = u32::from_le_bytes(data[4..8].try_into().ok()?);
    Some((pid, size, matches!(opcode, 10 | 26)))
}

pub fn encode(frame: &Frame) -> Option<Vec<u8>> {
    if frame.processes.len() > MAX_PROCESSES || frame.elapsed_ns == 0 {
        return None;
    }
    let mut data = Vec::with_capacity(40 + 32 * frame.processes.len());
    data.extend_from_slice(MAGIC);
    data.extend_from_slice(&frame.elapsed_ns.to_le_bytes());
    data.extend_from_slice(&frame.captured_at.to_le_bytes());
    data.extend_from_slice(&frame.lost.to_le_bytes());
    data.extend_from_slice(&(frame.processes.len() as u32).to_le_bytes());
    data.extend_from_slice(&0_u32.to_le_bytes());
    for entry in &frame.processes {
        data.extend_from_slice(&entry.pid.to_le_bytes());
        data.extend_from_slice(&0_u32.to_le_bytes());
        data.extend_from_slice(&entry.created.to_le_bytes());
        data.extend_from_slice(&entry.uploaded.to_le_bytes());
        data.extend_from_slice(&entry.downloaded.to_le_bytes());
    }
    Some(data)
}

pub fn decode(data: &[u8]) -> Option<Frame> {
    if data.len() < 40 || data.len() > MAX_MESSAGE || &data[..8] != MAGIC {
        return None;
    }
    let value = |offset| {
        Some(u64::from_le_bytes(
            data.get(offset..offset + 8)?.try_into().ok()?,
        ))
    };
    let count = u32::from_le_bytes(data[32..36].try_into().ok()?) as usize;
    if count > MAX_PROCESSES || data.len() != 40 + count * 32 {
        return None;
    }
    let elapsed_ns = value(8)?;
    if elapsed_ns == 0 || elapsed_ns > 60_000_000_000 {
        return None;
    }
    let mut processes = Vec::with_capacity(count);
    let mut pids = std::collections::HashSet::with_capacity(count);
    for offset in (40..data.len()).step_by(32) {
        let pid = u32::from_le_bytes(data[offset..offset + 4].try_into().ok()?);
        let created = value(offset + 8)?;
        if pid == 0 || created == 0 || !pids.insert(pid) {
            return None;
        }
        processes.push(Traffic {
            pid,
            created,
            uploaded: value(offset + 16)?,
            downloaded: value(offset + 24)?,
        });
    }
    Some(Frame {
        elapsed_ns,
        captured_at: value(16)?,
        lost: value(24)?,
        processes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_documented_data_event_prefix() {
        let payload = [42, 0, 0, 0, 0, 4, 0, 0];
        assert_eq!(parse_ip_event(2, 10, &payload), Some((42, 1024, true)));
        assert_eq!(parse_ip_event(2, 27, &payload), Some((42, 1024, false)));
        assert_eq!(parse_ip_event(2, 14, &payload), None); // TCP retransmit isn't new data.
        assert_eq!(parse_ip_event(0, 10, &payload), None); // Old schema has different fields.
        assert_eq!(parse_ip_event(2, 10, &payload[..7]), None);
    }

    #[test]
    fn protocol_rejects_truncation_oversize_and_invalid_lifetimes() {
        let frame = Frame {
            elapsed_ns: 2_000_000_000,
            captured_at: 123,
            lost: 0,
            processes: vec![Traffic {
                pid: 7,
                created: 100,
                uploaded: 103,
                downloaded: 42,
            }],
        };
        let encoded = encode(&frame).unwrap();
        assert_eq!(decode(&encoded).unwrap().processes, frame.processes);
        assert!(decode(&encoded[..encoded.len() - 1]).is_none());
        let mut invalid = encoded.clone();
        invalid[32..36].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode(&invalid).is_none());
        let mut invalid = encoded;
        invalid[48..56].fill(0);
        assert!(decode(&invalid).is_none());
    }
}
