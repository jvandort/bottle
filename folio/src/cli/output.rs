//! How folio shows things: tables, sizes, times.

use super::*;

pub(super) fn human_size(size: u64) -> String {
    for (unit, scale) in [("G", 1u64 << 30), ("M", 1 << 20), ("K", 1 << 10)] {
        if size >= scale {
            return format!("{:.1}{unit}", size as f64 / scale as f64);
        }
    }
    format!("{size}B")
}

pub(super) fn ago(when: f64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let seconds = (now - when).max(0.0) as u64;
    for (unit, size) in [("d", 86400), ("h", 3600), ("m", 60)] {
        if seconds >= size {
            return format!("{}{unit} ago", seconds / size);
        }
    }
    "just now".into()
}

pub(super) fn table(rows: &[Vec<String>]) {
    let widths: Vec<usize> = (0..rows[0].len())
        .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
        .collect();
    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, w)| format!("{cell:<w$}"))
            .collect();
        println!("{}", cells.join("  ").trim_end());
    }
}
