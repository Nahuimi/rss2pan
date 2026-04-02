use std::{
    fs,
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::Command,
};

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
use std::io::ErrorKind;

use url::Url;

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

#[cfg(test)]
mod tests {
    use super::canonicalize_magnet;

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
}
