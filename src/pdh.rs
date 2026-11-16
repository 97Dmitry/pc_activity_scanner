//! Bounded, RAII-owned PDH query. Construct/use/drop on a sampling worker.
use std::ptr;
use windows_sys::Win32::System::Performance::*;

const MAX_BUFFER_BYTES: usize = 8 * 1024 * 1024;
const MAX_ITEMS: usize = 65_536;

pub struct Query {
    handle: PDH_HQUERY,
    counters: Vec<Result<PDH_HCOUNTER, String>>,
}
impl Query {
    pub fn new(paths: &[&str]) -> Result<Self, String> {
        if paths.len() > 16 {
            return Err("PDH: too many counters".into());
        }
        let mut handle = ptr::null_mut();
        let status = unsafe { PdhOpenQueryW(ptr::null(), 0, &mut handle) };
        if status != 0 {
            return Err(format!("PdhOpenQuery: 0x{status:08x}"));
        }
        let mut query = Self {
            handle,
            counters: Vec::with_capacity(paths.len()),
        };
        for path in paths {
            let path: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
            let mut counter = ptr::null_mut();
            let status = unsafe { PdhAddEnglishCounterW(handle, path.as_ptr(), 0, &mut counter) };
            query.counters.push(if status == 0 {
                Ok(counter)
            } else {
                Err(format!("PdhAddEnglishCounter: 0x{status:08x}"))
            });
        }
        Ok(query)
    }
    pub fn collect(&mut self) -> Result<(), String> {
        let status = unsafe { PdhCollectQueryData(self.handle) };
        if status == 0 {
            Ok(())
        } else {
            Err(format!("PdhCollectQueryData: 0x{status:08x}"))
        }
    }
    pub fn values(&self, index: usize) -> Result<Vec<(String, f64)>, String> {
        let counter = self
            .counters
            .get(index)
            .ok_or("PDH counter index out of range")?
            .as_ref()
            .map_err(Clone::clone)?;
        let mut bytes = 0;
        let mut count = 0;
        let status = unsafe {
            PdhGetFormattedCounterArrayW(
                *counter,
                PDH_FMT_DOUBLE,
                &mut bytes,
                &mut count,
                ptr::null_mut(),
            )
        };
        if status != PDH_MORE_DATA {
            return Err(format!("PdhGetFormattedCounterArray(size): 0x{status:08x}"));
        }
        for _ in 0..3 {
            if bytes as usize > MAX_BUFFER_BYTES || bytes == 0 {
                return Err("PDH counter buffer exceeds limit".into());
            }
            // PDH returns both structs and UTF-16 strings in this allocation.
            // u64 provides the alignment required by the x64 PDH structs.
            let mut buffer = vec![0_u64; (bytes as usize).div_ceil(8)];
            let allocated = buffer.len() * 8;
            bytes = allocated as u32;
            let status = unsafe {
                PdhGetFormattedCounterArrayW(
                    *counter,
                    PDH_FMT_DOUBLE,
                    &mut bytes,
                    &mut count,
                    buffer.as_mut_ptr().cast(),
                )
            };
            if status == PDH_MORE_DATA {
                continue;
            }
            if status != 0 {
                return Err(format!("PdhGetFormattedCounterArray: 0x{status:08x}"));
            }
            if count as usize > MAX_ITEMS
                || count as usize > allocated / std::mem::size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>()
            {
                return Err("PDH counter count exceeds buffer".into());
            }
            let items = unsafe {
                std::slice::from_raw_parts(
                    buffer.as_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>(),
                    count as usize,
                )
            };
            let mut result = Vec::with_capacity(items.len());
            for item in items {
                if ![PDH_CSTATUS_VALID_DATA, PDH_CSTATUS_NEW_DATA].contains(&item.FmtValue.CStatus)
                {
                    continue;
                }
                let value = unsafe { item.FmtValue.Anonymous.doubleValue };
                if !value.is_finite() || value < 0.0 {
                    continue;
                }
                let start = buffer.as_ptr() as usize;
                let address = item.szName as usize;
                let end = start + allocated;
                if address < start || address >= end || !address.is_multiple_of(2) {
                    return Err("PDH instance name outside buffer".into());
                }
                let name = unsafe { std::slice::from_raw_parts(item.szName, (end - address) / 2) };
                let length = name
                    .iter()
                    .position(|ch| *ch == 0)
                    .ok_or("PDH instance name has no terminator")?;
                if length > 4096 {
                    return Err("PDH instance name exceeds limit".into());
                }
                result.push((String::from_utf16_lossy(&name[..length]), value));
            }
            return Ok(result);
        }
        Err("PDH instances changed repeatedly while reading".into())
    }
}
impl Drop for Query {
    fn drop(&mut self) {
        unsafe {
            PdhCloseQuery(self.handle);
        }
    }
}
