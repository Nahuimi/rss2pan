use std::{
    fs,
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::Command,
};

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
use std::io::ErrorKind;

pub fn get_magnet_list_by_txt(txt: &PathBuf) -> anyhow::Result<Vec<String>> {
    let mut magnet_list = Vec::new();
    let file = File::open(txt)?;
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = line?;
        if line.starts_with("magnet:")
            || line.starts_with("ed2k://")
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
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        Command::new("open").arg(path).spawn()?;
        return Ok(());
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
