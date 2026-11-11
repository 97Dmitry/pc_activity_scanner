use crate::model::ProcessRow;
use directories::UserDirs;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Export exactly the currently displayed, sorted rows. CSV is UTF-8+BOM, semicolon-delimited.
/// Files are created exclusively so two exports cannot silently overwrite one another.
pub fn export_csv(rows: &[ProcessRow]) -> Result<PathBuf, String> {
    let dir = UserDirs::new()
        .and_then(|user| user.download_dir().map(ToOwned::to_owned))
        .unwrap_or(std::env::current_dir().map_err(|e| e.to_string())?);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis();
    for suffix in 0..100 {
        let path = dir.join(format!("pc-activity-{timestamp}-{suffix}.csv"));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => {
                if let Err(err) = write_csv(file, rows) {
                    let _ = fs::remove_file(&path);
                    return Err(err);
                }
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    Err("Не удалось найти свободное имя файла экспорта".into())
}

/// Excel interprets text beginning with =, +, -, @ and control whitespace as formulas.
/// Prefix potentially executable contents with an apostrophe (text literal marker).
fn excel_safe(value: &str) -> String {
    let trimmed = value.trim_start_matches(|ch: char| matches!(ch, ' ' | '\t' | '\r' | '\n'));
    if trimmed.chars().next().is_some_and(|ch| matches!(ch, '=' | '+' | '-' | '@'))
        || value.chars().next().is_some_and(|ch| matches!(ch, '\t' | '\r' | '\n'))
    {
        format!("'{value}")
    } else {
        value.to_string()
    }
}

fn write_csv(mut output: impl Write, rows: &[ProcessRow]) -> Result<(), String> {
    output.write_all(b"\xEF\xBB\xBF").map_err(|e| e.to_string())?;
    let mut csv = csv::WriterBuilder::new().delimiter(b';').from_writer(output);
    csv.write_record([
        "Название", "PID", "Количество процессов", "Путь", "CPU %", "GPU %",
        "GPU RAM (байт)", "RAM (байт)", "Загрузка (байт/с)", "Отдача (байт/с)",
        "Загружено за сессию (байт)", "Отдано за сессию (байт)",
    ]).map_err(|e| e.to_string())?;
    for row in rows {
        csv.write_record([
            excel_safe(&row.name), row.pid.to_string(), row.count.to_string(),
            excel_safe(&row.executable), format!("{:.2}", row.cpu_pct),
            row.gpu_pct.map(|v| format!("{v:.2}")).unwrap_or_default(),
            row.gpu_memory_bytes.map(|v| v.to_string()).unwrap_or_default(),
            row.ram_bytes.to_string(),
            row.down_bps.map(|v| format!("{v:.1}")).unwrap_or_default(),
            row.up_bps.map(|v| format!("{v:.1}")).unwrap_or_default(),
            row.downloaded_bytes.map(|v| v.to_string()).unwrap_or_default(),
            row.uploaded_bytes.map(|v| v.to_string()).unwrap_or_default(),
        ]).map_err(|e| e.to_string())?;
    }
    csv.flush().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_roundtrip_preserves_order_and_escapes_formulas() {
        let sample = ProcessRow {
            name: "=HYPERLINK(\"http://example.com\")".into(),
            executable: "+cmd;name".into(),
            pid: 42,
            down_bps: None,
            ..Default::default()
        };
        let mut bytes = Vec::new();
        write_csv(&mut bytes, &[sample]).unwrap();
        assert_eq!(&bytes[..3], b"\xEF\xBB\xBF");
        let mut reader = csv::ReaderBuilder::new()
            .delimiter(b';')
            .from_reader(&bytes[3..]);
        let records = reader.records().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].get(0), Some("'=HYPERLINK(\"http://example.com\")"));
        assert_eq!(records[0].get(1), Some("42"));
        assert_eq!(records[0].get(3), Some("'+cmd;name"));
        assert_eq!(records[0].get(8), Some(""));
    }

    #[test]
    fn safe_cells_are_not_altered() {
        assert_eq!(excel_safe("firefox"), "firefox");
        assert_eq!(excel_safe("-3"), "'-3");
        assert_eq!(excel_safe("  =1+1"), "'  =1+1");
    }
}
