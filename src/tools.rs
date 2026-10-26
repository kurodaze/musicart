//! yt-dlp, and deno unless one is on PATH, downloaded into `data/bin` on first use and checked
//! against the SHA-256 their releases publish. Delete one to get a fresh copy.

use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;
use tracing::info;

pub const YTDLP: &str = "data/bin/yt-dlp";
const DENO: &str = "data/bin/deno";
/// Tens of MB: a slow link needs longer than a lookup's timeouts.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
/// More than any release is, so a wrong answer can't fill the memory.
const MAX_DOWNLOAD: usize = 200 * 1024 * 1024;

static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .unwrap_or_default()
});

/// A packaged deno (the container's) wins, since deno only publishes glibc builds.
static SYSTEM_DENO: LazyLock<bool> = LazyLock::new(|| {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("deno").is_file()))
});

/// Makes sure yt-dlp and deno are there, downloading what's missing, and returns what yt-dlp is
/// told with `--js-runtimes`.
pub async fn ready() -> Result<String, String> {
    static INSTALL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let missing = |path: &str| !Path::new(path).is_file();
    if missing(YTDLP) || (!*SYSTEM_DENO && missing(DENO)) {
        let _turn = INSTALL.lock().await;
        let arch = match std::env::consts::ARCH {
            arch @ ("x86_64" | "aarch64") => arch,
            other => return Err(format!("no yt-dlp or deno download for {other}")),
        };
        if missing(YTDLP) {
            // each build runs only on its own libc, which the system's ldd names
            let musl = fs::read_to_string("/usr/bin/ldd").is_ok_and(|s| s.contains("musl"));
            let libc = if musl { "musllinux" } else { "linux" };
            let name = match arch {
                "aarch64" => format!("yt-dlp_{libc}_aarch64"),
                _ => format!("yt-dlp_{libc}"),
            };
            let base = "https://github.com/yt-dlp/yt-dlp/releases/latest/download";
            let bin = download(
                &format!("{base}/{name}"),
                &format!("{base}/SHA2-256SUMS"),
                &name,
            )
            .await?;
            install(YTDLP, &bin)?;
        }
        if !*SYSTEM_DENO && missing(DENO) {
            let name = format!("deno-{arch}-unknown-linux-gnu.zip");
            let url = format!("https://github.com/denoland/deno/releases/latest/download/{name}");
            let zip = download(&url, &format!("{url}.sha256sum"), &name).await?;
            install(DENO, &unzip_single(&zip)?)?;
        }
    }
    Ok(if *SYSTEM_DENO {
        "deno".into()
    } else {
        let path = fs::canonicalize(DENO).unwrap_or_else(|_| PathBuf::from(DENO));
        format!("deno:{}", path.display())
    })
}

/// A release asset, checked against its published SHA-256.
async fn download(url: &str, sums_url: &str, name: &str) -> Result<Vec<u8>, String> {
    info!("downloading {url}");
    let sums = fetch(sums_url).await?;
    let expected = parse_sha256sums(&String::from_utf8_lossy(&sums), name)
        .ok_or_else(|| format!("no checksum for {name}"))?;
    let bytes = fetch(url).await?;
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if actual != expected {
        return Err(format!(
            "{name} doesn't match its checksum (expected {expected}, got {actual})"
        ));
    }
    Ok(bytes)
}

async fn fetch(url: &str) -> Result<Vec<u8>, String> {
    let error = |e: reqwest::Error| format!("{url}: {e}");
    let mut response = HTTP
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(error)?;
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(error)? {
        if body.len() + chunk.len() > MAX_DOWNLOAD {
            return Err(format!("{url} is larger than it should be"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Writes an executable through a temp file, so an interrupted install never looks complete.
fn install(path: &str, bytes: &[u8]) -> Result<(), String> {
    let tmp = Path::new(path).with_extension("tmp");
    if let Some(folder) = tmp.parent() {
        fs::create_dir_all(folder).map_err(|e| format!("{}: {e}", folder.display()))?;
    }
    fs::write(&tmp, bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| format!("{path}: {e}"))
}

/// The single deflated entry of a deno release zip; the zip's checksum vouches for it.
fn unzip_single(zip: &[u8]) -> Result<Vec<u8>, String> {
    let le16 = |i: usize| {
        zip.get(i..i + 2)
            .map_or(0, |b| usize::from(b[0]) | usize::from(b[1]) << 8)
    };
    if !zip.starts_with(b"PK\x03\x04") || le16(8) != 8 {
        return Err("deno's release isn't a deflated zip".into());
    }
    // the local header is 30 bytes, then the file name and the extra field
    let data = zip.get(30 + le16(26) + le16(28)..).unwrap_or_default();
    let mut out = Vec::new();
    flate2::read::DeflateDecoder::new(data)
        .read_to_end(&mut out)
        .map_err(|e| format!("unpacking deno: {e}"))?;
    Ok(out)
}

/// The hash for `filename` in a sums file: GNU `hash  name` lines (the name may carry a path or a
/// `*`), or a file holding just the hash.
fn parse_sha256sums(text: &str, filename: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let named = parts.next().map(|n| n.trim_start_matches('*'));
        let ok = hash.len() == 64
            && hash.bytes().all(|b| b.is_ascii_hexdigit())
            && named.is_none_or(|n| n == filename || n.ends_with(&format!("/{filename}")));
        ok.then(|| hash.to_ascii_lowercase())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unzip_single_inflates_the_only_entry() {
        use std::io::Write;
        let mut deflate = flate2::write::DeflateEncoder::new(Vec::new(), Default::default());
        deflate.write_all(b"deno binary").unwrap();
        let mut zip = b"PK\x03\x04\x14\0\0\0\x08\0".to_vec(); // version, flags, deflate
        zip.resize(26, 0);
        zip.extend(b"\x04\0\0\0deno"); // name and extra lengths, name
        zip.extend(deflate.finish().unwrap());
        assert_eq!(unzip_single(&zip).unwrap(), b"deno binary");
        assert!(unzip_single(b"not a zip").is_err());
    }

    #[test]
    fn checksums_by_name() {
        let text = "\
6bbb3d314cde4febe36e5fa1d55462e29c974f63444e707871834f6d8cc210ae  yt-dlp_linux
b6ce97646773070d7a7ffd6bbbdcaecb47c48483909c54c915bf08a7a9b5e0b1  yt-dlp_linux_aarch64
";
        assert_eq!(
            parse_sha256sums(text, "yt-dlp_linux").as_deref(),
            Some("6bbb3d314cde4febe36e5fa1d55462e29c974f63444e707871834f6d8cc210ae")
        );
        assert!(parse_sha256sums(text, "yt-dlp.exe").is_none());
        assert!(parse_sha256sums("not-a-hash  yt-dlp_linux", "yt-dlp_linux").is_none());
        // deno publishes one hash per asset, sometimes bare
        let bare = "6BBB3D314CDE4FEBE36E5FA1D55462E29C974F63444E707871834F6D8CC210AE\n";
        assert_eq!(
            parse_sha256sums(bare, "deno.zip").as_deref(),
            Some("6bbb3d314cde4febe36e5fa1d55462e29c974f63444e707871834f6d8cc210ae")
        );
    }
}
