use crate::model::ProcessRow;
use directories::UserDirs;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Export precisely the currently visible, sorted rows. Excel opens UTF-8 BOM + semicolon CSV.
pub fn export_csv(rows: &[ProcessRow]) -> Result<PathBuf, String> {
    let dir = UserDirs::new()
        .and_then(|user| user.download_dir().map(ToOwned::to_owned))
        .unwrap_or(std::env::current_dir().map_err(|e| e.to_string())?);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs();
    let file = dir.join(format!("pc-activity-{now}.csv"));
    let mut output = File::create(&file).map_err(|e| e.to_string())?;
    output.write_all(b"\xEF\xBB\xBF").map_err(|e| e.to_string())?;
    let mut csv = csv::WriterBuilder::new().delimiter(b';').from_writer(output);
    csv.write_record([
        "Название", "PID", "Количество процессов", "Путь", "CPU %", "GPU %",
        "GPU RAM (байт)", "RAM (байт)", "Загрузка (байт/с)", "Отдача (байт/с)",
        "Загружено за сессию (байт)", "Отдано за сессию (байт)",
    ]).map_err(|e| e.to_string())?;
    for row in rows {
        csv.write_record([
            row.name.clone(), row.pid.to_string(), row.count.to_string(), row.executable.clone(),
            format!("{:.2}", row.cpu_pct),
            row.gpu_pct.map(|v| format!("{v:.2}")).unwrap_or_default(),
            row.gpu_memory_bytes.map(|v| v.to_string()).unwrap_or_default(),
            row.ram_bytes.to_string(),
            row.down_bps.map(|v| format!("{v:.1}")).unwrap_or_default(),
            row.up_bps.map(|v| format!("{v:.1}")).unwrap_or_default(),
            row.downloaded_bytes.map(|v| v.to_string()).unwrap_or_default(),
            row.uploaded_bytes.map(|v| v.to_string()).unwrap_or_default(),
        ]).map_err(|e| e.to_string())?;
    }
    csv.flush().map_err(|e| e.to_string())?;
    Ok(file)
}
