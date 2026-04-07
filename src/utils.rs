use std::{
    fs,
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::Command,
};

use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc};

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
use std::io::ErrorKind;

use url::Url;

const DB_TIMESTAMP_FORMAT: &str = "%Y-%m-%d %H：%M";
const BEIJING_UTC_OFFSET_SECONDS: i32 = 8 * 60 * 60;

pub fn canonicalize_magnet(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let Ok(url) = Url::parse(trimmed) else {
        return trimmed.to_string();
    };
    if !url.scheme().eq_ignore_ascii_case("magnet") {
        return trimmed.to_string();
    }

    let Some(xt) = url.query_pairs().find_map(|(key, value)| {
        (key.eq_ignore_ascii_case("xt") && !value.trim().is_empty()).then(|| value.into_owned())
    }) else {
        return trimmed.to_string();
    };

    canonicalize_btih_xt(&xt)
        .map(|xt| format!("magnet:?xt={xt}"))
        .unwrap_or_else(|| trimmed.to_string())
}

fn canonicalize_btih_xt(xt: &str) -> Option<String> {
    let mut parts = xt.trim().splitn(3, ':');
    let urn = parts.next()?;
    let kind = parts.next()?;
    let hash = parts.next()?.trim();
    if !urn.eq_ignore_ascii_case("urn") || !kind.eq_ignore_ascii_case("btih") || hash.is_empty() {
        return None;
    }
    Some(format!("urn:btih:{}", hash.to_ascii_lowercase()))
}

pub fn get_magnet_list_by_txt(txt: &PathBuf) -> anyhow::Result<Vec<String>> {
    let mut magnet_list = Vec::new();
    let file = File::open(txt)?;
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = line?;
        if line.starts_with("magnet:") {
            magnet_list.push(canonicalize_magnet(&line));
        } else if line.starts_with("ed2k://")
            || line.starts_with("https://")
            || line.starts_with("http://")
            || line.starts_with("ftp://")
            || line.starts_with("ftps://")
        {
            magnet_list.push(line.trim_end().to_string());
        }
    }
    Ok(magnet_list)
}

pub fn write_qrcode_image(image: &[u8]) -> anyhow::Result<PathBuf> {
    let path = PathBuf::from("qrcode115.png");
    fs::write(&path, image)?;
    Ok(path)
}

pub fn remove_qrcode_image(path: &Path) {
    let _ = fs::remove_file(path);
}

pub fn open_qrcode_image(path: &Path) -> anyhow::Result<()> {
    #[cfg(target_os = "windows")]
    {
        Command::new("cmd")
            .arg("/C")
            .arg("start")
            .arg("")
            .arg(path)
            .spawn()?;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    {
        Command::new("open").arg(path).spawn()?;
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        for program in ["xdg-open", "gnome-open", "kde-open"] {
            match Command::new(program).arg(path).spawn() {
                Ok(_) => return Ok(()),
                Err(err) if err.kind() == ErrorKind::NotFound => continue,
                Err(err) => return Err(err.into()),
            }
        }
        anyhow::bail!("no open command found")
    }

    #[cfg(not(any(
        target_os = "windows",
        target_os = "macos",
        target_os = "linux",
        target_os = "freebsd"
    )))]
    {
        let _ = path;
        anyhow::bail!("unsupported platform for opening qrcode image")
    }
}

pub fn current_db_timestamp() -> String {
    format_db_timestamp(Utc::now())
}

pub fn format_db_timestamp<Tz: TimeZone>(datetime: DateTime<Tz>) -> String {
    datetime
        .with_timezone(&beijing_offset())
        .format(DB_TIMESTAMP_FORMAT)
        .to_string()
}

pub fn normalize_db_timestamp(raw: &str) -> Option<String> {
    parse_db_timestamp(raw).map(format_db_timestamp)
}

pub fn db_timestamp_months_ago(months: u32) -> String {
    format_db_timestamp(subtract_months(
        Utc::now().with_timezone(&beijing_offset()),
        months,
    ))
}

fn parse_db_timestamp(raw: &str) -> Option<DateTime<FixedOffset>> {
    let normalized = normalize_db_timestamp_input(raw);
    for candidate in timestamp_parse_candidates(&normalized) {
        if let Ok(datetime) = DateTime::parse_from_rfc3339(&candidate) {
            return Some(datetime);
        }
        for fmt in [
            "%Y-%m-%d %H:%M:%S%.f%:z",
            "%Y-%m-%d %H:%M:%S%:z",
            "%Y-%m-%d %H:%M%:z",
            "%Y-%m-%d %H:%M:%S%.f %:z",
            "%Y-%m-%d %H:%M:%S %:z",
            "%Y-%m-%d %H:%M %:z",
        ] {
            if let Ok(datetime) = DateTime::parse_from_str(&candidate, fmt) {
                return Some(datetime);
            }
        }
        for fmt in [
            "%Y-%m-%d %H:%M:%S%.f",
            "%Y-%m-%d %H:%M:%S",
            "%Y-%m-%d %H:%M",
        ] {
            if let Ok(datetime) = NaiveDateTime::parse_from_str(&candidate, fmt) {
                if let Some(datetime) = beijing_offset().from_local_datetime(&datetime).single() {
                    return Some(datetime);
                }
            }
        }
    }
    None
}

fn normalize_db_timestamp_input(raw: &str) -> String {
    let mut normalized = raw.trim().replace('：', ":").replace('\u{3000}', " ");
    normalized = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.len() > 10 && normalized.as_bytes()[10].is_ascii_digit() {
        normalized.insert(10, ' ');
    }
    normalized
}

fn timestamp_parse_candidates(normalized: &str) -> Vec<String> {
    let mut candidates = vec![normalized.to_string()];

    if normalized.ends_with(" UTC") {
        candidates.push(format!("{}+00:00", normalized.trim_end_matches(" UTC")));
    }

    if normalized.len() > 10 {
        match normalized.as_bytes()[10] {
            b' ' => {
                let mut rfc3339 = normalized.to_string();
                rfc3339.replace_range(10..11, "T");
                candidates.push(rfc3339);
            }
            b'T' => {
                let mut spaced = normalized.to_string();
                spaced.replace_range(10..11, " ");
                candidates.push(spaced);
            }
            _ => {}
        }
    }

    candidates.sort();
    candidates.dedup();
    candidates
}

fn subtract_months(datetime: DateTime<FixedOffset>, months: u32) -> DateTime<FixedOffset> {
    let total_months = i64::from(datetime.year()) * 12 + i64::from(datetime.month0());
    let adjusted_months = total_months - i64::from(months);
    let year = adjusted_months.div_euclid(12) as i32;
    let month0 = adjusted_months.rem_euclid(12) as u32;
    let month = month0 + 1;
    let day = datetime.day().min(last_day_of_month(year, month));
    let naive = NaiveDate::from_ymd_opt(year, month, day)
        .and_then(|date| date.and_hms_opt(datetime.hour(), datetime.minute(), 0))
        .expect("valid Beijing timestamp");
    beijing_offset()
        .from_local_datetime(&naive)
        .single()
        .expect("valid Beijing timestamp")
}

fn last_day_of_month(year: i32, month: u32) -> u32 {
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let first_day_next_month =
        NaiveDate::from_ymd_opt(next_year, next_month, 1).expect("valid first day of month");
    first_day_next_month
        .pred_opt()
        .expect("previous day exists")
        .day()
}

fn beijing_offset() -> FixedOffset {
    FixedOffset::east_opt(BEIJING_UTC_OFFSET_SECONDS).expect("valid Beijing offset")
}

#[cfg(test)]
mod tests {
    use super::{
        canonicalize_magnet, db_timestamp_months_ago, format_db_timestamp, normalize_db_timestamp,
    };
    use chrono::{TimeZone, Utc};

    #[test]
    fn test_canonicalize_magnet_strips_extra_params_and_lowercases_hash() {
        assert_eq!(
            canonicalize_magnet(" magnet:?dn=test&xt=URN:BTIH:ABCDEF123456&tr=udp://tracker "),
            "magnet:?xt=urn:btih:abcdef123456"
        );
    }

    #[test]
    fn test_canonicalize_magnet_keeps_non_magnet_links() {
        assert_eq!(
            canonicalize_magnet(" https://example.com/file.torrent "),
            "https://example.com/file.torrent"
        );
    }

    #[test]
    fn test_normalize_db_timestamp_accepts_mixed_legacy_formats() {
        assert_eq!(
            normalize_db_timestamp("2026-04-07T03:13:16.728787900+00:00").as_deref(),
            Some("2026-04-07 11：13")
        );
        assert_eq!(
            normalize_db_timestamp("2024-05-2115:18:14.3284712+08:00").as_deref(),
            Some("2024-05-21 15：18")
        );
        assert_eq!(
            normalize_db_timestamp("2026-04-05 07：57：40.658769500 UTC").as_deref(),
            Some("2026-04-05 15：57")
        );
    }

    #[test]
    fn test_format_db_timestamp_uses_beijing_time_and_fullwidth_colon() {
        let datetime = Utc.with_ymd_and_hms(2026, 4, 5, 7, 57, 40).unwrap();
        assert_eq!(format_db_timestamp(datetime), "2026-04-05 15：57");
    }

    #[test]
    fn test_db_timestamp_months_ago_keeps_db_format() {
        let timestamp = db_timestamp_months_ago(6);
        assert_eq!(timestamp.len(), "2026-04-05 07：57".len());
        assert!(timestamp.contains('：'));
    }
}
