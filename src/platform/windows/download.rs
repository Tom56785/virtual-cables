//! Downloads the latest usbip-win2 installer from its official GitHub
//! releases, over WinHTTP.

use std::fmt;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use serde_json::Value;
use windows_sys::Win32::Networking::WinHttp::*;
use windows_sys::Win32::System::Diagnostics::Debug::{
    FORMAT_MESSAGE_FROM_HMODULE, FORMAT_MESSAGE_FROM_SYSTEM, FORMAT_MESSAGE_IGNORE_INSERTS, FormatMessageW,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;

use super::win::wide;

const API_URL: &str = "https://api.github.com/repos/vadimgrn/usbip-win2/releases/latest";
const USER_AGENT: &str = "Virtual-Cables";
const CLIENT_TIMEOUT: Duration = Duration::from_secs(4 * 60);
const MAX_REDIRECTS: usize = 10;
const ERROR_WINHTTP_NAME_NOT_RESOLVED: u32 = 12007;

#[derive(Debug)]
pub struct DownloadError {
    pub message: String,
    /// The host name could not be resolved (a DNS problem).
    pub name_resolution: bool,
}

impl DownloadError {
    fn new(message: impl Into<String>) -> DownloadError {
        DownloadError { message: message.into(), name_resolution: false }
    }

    fn context(self, context: &str) -> DownloadError {
        DownloadError { message: format!("{context}: {}", self.message), ..self }
    }
}

impl fmt::Display for DownloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

pub fn is_name_resolution_error(err: &DownloadError) -> bool {
    if err.name_resolution {
        return true;
    }
    let text = err.message.to_lowercase();
    ["no such host", "name resolution", "servername oder serveradresse", "host is unknown"]
        .iter()
        .any(|pattern| text.contains(pattern))
}

/// Text for a WinHTTP or system error code.
fn error_text(code: u32) -> String {
    let mut buf = [0u16; 512];
    // SAFETY: FormatMessageW writes at most buf.len() characters into buf.
    let len = unsafe {
        let module = GetModuleHandleW(wide("winhttp.dll").as_ptr());
        FormatMessageW(
            FORMAT_MESSAGE_FROM_HMODULE | FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
            module as *const _,
            code,
            0,
            buf.as_mut_ptr(),
            buf.len() as u32,
            null(),
        )
    };
    let text = String::from_utf16_lossy(&buf[..len as usize]);
    let text = text.trim();
    if text.is_empty() { format!("error {code}") } else { format!("{text} (error {code})") }
}

fn last_winhttp_error() -> DownloadError {
    let code = std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as u32;
    DownloadError { message: error_text(code), name_resolution: code == ERROR_WINHTTP_NAME_NOT_RESOLVED }
}

/// A WinHTTP handle, closed on drop.
struct Handle(*mut std::ffi::c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the handle came from WinHTTP and is closed once.
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

/// A response whose body can be read.
pub struct Response {
    pub status: u32,
    pub status_text: String,
    deadline: Instant,
    request: Handle,
    _connect: Handle,
    _session: Handle,
}

impl Response {
    /// "200 OK", as the status line shows it.
    fn status_line(&self) -> String {
        format!("{} {}", self.status, self.status_text).trim().to_string()
    }

    /// Reads up to `buf.len()` body bytes; 0 means the body ended.
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, DownloadError> {
        if Instant::now() >= self.deadline {
            return Err(DownloadError::new("download timed out"));
        }
        let mut read = 0u32;
        // SAFETY: WinHttpReadData writes at most buf.len() bytes into buf.
        if unsafe { WinHttpReadData(self.request.0, buf.as_mut_ptr().cast(), buf.len() as u32, &mut read) } == 0 {
            return Err(last_winhttp_error());
        }
        Ok(read as usize)
    }

    fn read_to_end(&mut self, limit: usize) -> Result<Vec<u8>, DownloadError> {
        let mut body = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];
        while body.len() < limit {
            let n = self.read(&mut buf)?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&buf[..n.min(limit - body.len())]);
        }
        Ok(body)
    }
}

struct Url {
    https: bool,
    host: String,
    port: u16,
    path: String,
}

fn crack_url(url: &str) -> Result<Url, DownloadError> {
    let wide_url = wide(url);
    let mut host = vec![0u16; 1024];
    let mut path = vec![0u16; 8192];
    let mut extra = vec![0u16; 8192];
    // SAFETY: the component buffers and their lengths are set up as
    // WinHttpCrackUrl requires; URL_COMPONENTS is plain data.
    unsafe {
        let mut parts: URL_COMPONENTS = std::mem::zeroed();
        parts.dwStructSize = size_of::<URL_COMPONENTS>() as u32;
        parts.lpszHostName = host.as_mut_ptr();
        parts.dwHostNameLength = host.len() as u32;
        parts.lpszUrlPath = path.as_mut_ptr();
        parts.dwUrlPathLength = path.len() as u32;
        parts.lpszExtraInfo = extra.as_mut_ptr();
        parts.dwExtraInfoLength = extra.len() as u32;
        if WinHttpCrackUrl(wide_url.as_ptr(), 0, 0, &mut parts) == 0 {
            return Err(last_winhttp_error().context("invalid URL"));
        }
        let text = |buf: &[u16], len: u32| String::from_utf16_lossy(&buf[..len as usize]);
        Ok(Url {
            https: parts.nScheme == WINHTTP_INTERNET_SCHEME_HTTPS,
            host: text(&host, parts.dwHostNameLength),
            port: parts.nPort,
            path: text(&path, parts.dwUrlPathLength) + &text(&extra, parts.dwExtraInfoLength),
        })
    }
}

/// Resolves a redirect target against the URL that returned it.
fn resolve_location(base: &str, location: &str) -> String {
    if location.contains("://") {
        return location.to_string();
    }
    let scheme_end = base.find("://").map_or(0, |i| i + 3);
    let authority_end = base[scheme_end..].find('/').map_or(base.len(), |i| scheme_end + i);
    if location.starts_with("//") {
        return format!("{}{}", &base[..scheme_end - 2], location);
    }
    if location.starts_with('/') {
        return format!("{}{}", &base[..authority_end], location);
    }
    let directory_end = base.rfind('/').filter(|&i| i >= authority_end).map_or(authority_end, |i| i + 1);
    let prefix = &base[..directory_end];
    if prefix.ends_with('/') { format!("{prefix}{location}") } else { format!("{prefix}/{location}") }
}

/// GETs `url`, following up to ten redirects, each of which must pass the
/// GitHub host check.
fn get(url: &str, accept: Option<&str>, deadline: Instant) -> Result<Response, DownloadError> {
    let mut url = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        let response = get_once(&url, accept, deadline)?;
        if !matches!(response.status, 301 | 302 | 303 | 307 | 308) {
            return Ok(response);
        }
        let Some(location) = query_header(&response.request, WINHTTP_QUERY_LOCATION) else {
            return Ok(response);
        };
        url = resolve_location(&url, &location);
        validate_github_download_url(&url).map_err(DownloadError::new)?;
    }
    Err(DownloadError::new("too many HTTP redirects"))
}

fn query_header(request: &Handle, info: u32) -> Option<String> {
    let mut buf = vec![0u16; 4096];
    let mut size = (buf.len() * 2) as u32;
    // SAFETY: WinHttpQueryHeaders writes at most `size` bytes into buf.
    let ok = unsafe { WinHttpQueryHeaders(request.0, info, null(), buf.as_mut_ptr().cast(), &mut size, null_mut()) };
    (ok != 0).then(|| String::from_utf16_lossy(&buf[..size as usize / 2]))
}

fn get_once(url: &str, accept: Option<&str>, deadline: Instant) -> Result<Response, DownloadError> {
    let parts = crack_url(url)?;
    if !parts.https {
        return Err(DownloadError::new("the driver download is not HTTPS"));
    }
    let remaining_ms =
        || deadline.saturating_duration_since(Instant::now()).as_millis().clamp(1, i32::MAX as u128) as i32;
    // SAFETY: each call receives valid handles and NUL-terminated UTF-16
    // strings that outlive it; handles are closed by Handle's drop.
    unsafe {
        let mut session =
            Handle(WinHttpOpen(wide(USER_AGENT).as_ptr(), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, null(), null(), 0));
        if session.0.is_null() {
            // Automatic proxy detection needs Windows 8.1.
            session =
                Handle(WinHttpOpen(wide(USER_AGENT).as_ptr(), WINHTTP_ACCESS_TYPE_DEFAULT_PROXY, null(), null(), 0));
        }
        if session.0.is_null() {
            return Err(last_winhttp_error());
        }
        let connect = Handle(WinHttpConnect(session.0, wide(&parts.host).as_ptr(), parts.port, 0));
        if connect.0.is_null() {
            return Err(last_winhttp_error());
        }
        let request = Handle(WinHttpOpenRequest(
            connect.0,
            wide("GET").as_ptr(),
            wide(&parts.path).as_ptr(),
            null(),
            null(),
            null(),
            WINHTTP_FLAG_SECURE,
        ));
        if request.0.is_null() {
            return Err(last_winhttp_error());
        }
        // Redirects are followed by `get`, which checks every target host.
        let never: u32 = WINHTTP_OPTION_REDIRECT_POLICY_NEVER;
        WinHttpSetOption(request.0, WINHTTP_OPTION_REDIRECT_POLICY, (&never as *const u32).cast(), 4);
        let timeout = remaining_ms();
        WinHttpSetTimeouts(request.0, timeout, timeout, timeout, timeout);
        let headers = accept.map(|accept| wide(&format!("Accept: {accept}\r\n"))).unwrap_or_else(|| vec![0]);
        let header_len = if accept.is_some() { u32::MAX } else { 0 }; // -1: NUL-terminated
        if WinHttpSendRequest(request.0, headers.as_ptr(), header_len, null(), 0, 0, 0) == 0
            || WinHttpReceiveResponse(request.0, null_mut()) == 0
        {
            return Err(last_winhttp_error());
        }
        let mut status = 0u32;
        let mut size = 4u32;
        if WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            null(),
            (&mut status as *mut u32).cast(),
            &mut size,
            null_mut(),
        ) == 0
        {
            return Err(last_winhttp_error());
        }
        let status_text = query_header(&request, WINHTTP_QUERY_STATUS_TEXT).unwrap_or_default();
        Ok(Response { status, status_text, deadline, request, _connect: connect, _session: session })
    }
}

pub struct ReleaseAsset {
    pub name: String,
    pub url: String,
    pub size: i64,
}

/// Picks the best Windows x64 EXE or MSI installer from a release.
pub fn choose_windows_x64_installer(release: &Value) -> Result<ReleaseAsset, String> {
    let mut best_score = -1;
    let mut best = None;
    for asset in release["assets"].as_array().into_iter().flatten() {
        let name = asset["name"].as_str().unwrap_or_default();
        let url = asset["browser_download_url"].as_str().unwrap_or_default();
        let lower = name.to_lowercase();
        if url.is_empty() {
            continue;
        }
        if ["arm64", "aarch64", "symbols", "pdb", "source", "report"].iter().any(|s| lower.contains(s)) {
            continue;
        }
        let mut score = if lower.ends_with(".exe") {
            100
        } else if lower.ends_with(".msi") {
            95
        } else {
            continue;
        };
        if ["x64", "amd64", "win64"].iter().any(|s| lower.contains(s)) {
            score += 60;
        }
        if lower.contains("installer") || lower.contains("setup") {
            score += 30;
        }
        if lower.contains("usbip") {
            score += 10;
        }
        if score > best_score {
            best_score = score;
            best = Some(ReleaseAsset {
                name: name.to_string(),
                url: url.to_string(),
                size: asset["size"].as_i64().unwrap_or(0),
            });
        }
    }
    best.ok_or_else(|| {
        format!(
            "release {} does not contain a Windows x64 EXE or MSI installer; the official release page was opened instead",
            release["tag_name"].as_str().unwrap_or_default()
        )
    })
}

/// Accepts only HTTPS URLs on GitHub's download hosts.
pub fn validate_github_download_url(raw: &str) -> Result<(), String> {
    let Some((scheme, rest)) = raw.split_once("://") else {
        return Err("the driver download is not HTTPS".into());
    };
    if !scheme.eq_ignore_ascii_case("https") {
        return Err("the driver download is not HTTPS".into());
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let host = if let Some(bracketed) = host_port.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or_default()
    } else {
        match host_port.rsplit_once(':') {
            Some((host, port)) if port.bytes().all(|b| b.is_ascii_digit()) => host,
            _ => host_port,
        }
    };
    let host = host.to_lowercase();
    let allowed = host == "github.com"
        || host == "api.github.com"
        || host == "objects.githubusercontent.com"
        || host.ends_with(".githubusercontent.com");
    if !allowed {
        return Err(format!("refusing unexpected download host: {host}"));
    }
    Ok(())
}

/// Downloads the installer into `download_dir` and returns its path and file
/// name. `status` reports progress; `open_url` opens the release page when
/// the release has no suitable installer; `log` records the download URL.
pub fn download_latest_usbip_installer(
    download_dir: &Path,
    status: &dyn Fn(&str),
    open_url: &dyn Fn(&str),
    log: &dyn Fn(&str),
) -> Result<(PathBuf, String), DownloadError> {
    let deadline = Instant::now() + CLIENT_TIMEOUT;
    let mut response = get(API_URL, Some("application/vnd.github+json"), deadline)
        .map_err(|err| err.context("GitHub release query failed"))?;
    if response.status != 200 {
        return Err(DownloadError::new(format!("GitHub release query returned HTTP {}", response.status_line())));
    }
    let body = response.read_to_end(4 << 20).map_err(|err| err.context("invalid GitHub release response"))?;
    let release: Value = serde_json::from_slice(&body)
        .map_err(|err| DownloadError::new(format!("invalid GitHub release response: {err}")))?;

    let asset = match choose_windows_x64_installer(&release) {
        Ok(asset) => asset,
        Err(err) => {
            if let Some(page) = release["html_url"].as_str().filter(|p| !p.is_empty()) {
                open_url(page);
            }
            return Err(DownloadError::new(err));
        }
    };

    fs::create_dir_all(download_dir).map_err(|err| DownloadError::new(format!("create download folder: {err}")))?;
    let name = Path::new(&asset.name.replace('/', "\\"))
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty() && n != ".")
        .ok_or_else(|| DownloadError::new("the release asset has no valid filename"))?;
    let final_path = download_dir.join(&name);
    let temp_path = download_dir.join(format!("{name}.part"));
    let _ = fs::remove_file(&temp_path);

    status(&format!("Downloading {name}..."));
    validate_github_download_url(&asset.url).map_err(DownloadError::new)?;
    log(&format!("Downloading official GitHub release asset: {}", asset.url));
    let mut download = get(&asset.url, None, deadline).map_err(|err| err.context("installer download failed"))?;
    if download.status != 200 {
        return Err(DownloadError::new(format!("installer download returned HTTP {}", download.status_line())));
    }

    let mut out =
        File::create(&temp_path).map_err(|err| DownloadError::new(format!("create temporary installer: {err}")))?;
    let mut written: i64 = 0;
    let mut buf = vec![0u8; 64 * 1024];
    let copy_result = loop {
        match download.read(&mut buf) {
            Ok(0) => break Ok(()),
            Ok(n) => {
                if let Err(err) = out.write_all(&buf[..n]) {
                    break Err(err.to_string());
                }
                written += n as i64;
            }
            Err(err) => break Err(err.message),
        }
    };
    let close_result = out.sync_all();
    drop(out);
    let fail = |message: String| {
        let _ = fs::remove_file(&temp_path);
        Err(DownloadError::new(message))
    };
    if let Err(err) = copy_result {
        return fail(format!("write installer: {err}"));
    }
    if let Err(err) = close_result {
        return fail(format!("close installer: {err}"));
    }
    if asset.size > 0 && written != asset.size {
        return fail(format!("download size mismatch: expected {} bytes, received {written}", asset.size));
    }
    if written < 64 * 1024 {
        return fail(format!("downloaded installer is unexpectedly small: {written} bytes"));
    }

    let _ = fs::remove_file(&final_path);
    if let Err(err) = fs::rename(&temp_path, &final_path) {
        return fail(format!("finish installer download: {err}"));
    }
    Ok((final_path, name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accepts_only_github_https_hosts() {
        for ok in [
            "https://github.com/vadimgrn/usbip-win2/releases/download/v1/setup.exe",
            "https://api.github.com/repos/x",
            "HTTPS://objects.githubusercontent.com/a?b=c",
            "https://release-assets.githubusercontent.com:443/x",
            "https://user@github.com/x",
        ] {
            assert_eq!(validate_github_download_url(ok), Ok(()), "{ok}");
        }
        assert_eq!(validate_github_download_url("http://github.com/x"), Err("the driver download is not HTTPS".into()));
        assert_eq!(
            validate_github_download_url("https://evil.example.com/github.com"),
            Err("refusing unexpected download host: evil.example.com".into())
        );
        assert_eq!(
            validate_github_download_url("https://githubusercontent.com.evil.io/x"),
            Err("refusing unexpected download host: githubusercontent.com.evil.io".into())
        );
    }

    #[test]
    fn chooses_the_windows_x64_installer() {
        let release = json!({
            "tag_name": "v0.9",
            "assets": [
                {"name": "usbip-win2-arm64-setup.exe", "browser_download_url": "https://github.com/a", "size": 1},
                {"name": "USBip-0.9-x64.msi", "browser_download_url": "https://github.com/b", "size": 2},
                {"name": "USBip-0.9-x64-setup.exe", "browser_download_url": "https://github.com/c", "size": 3},
                {"name": "USBip-0.9-x64.pdb.zip", "browser_download_url": "https://github.com/d", "size": 4},
                {"name": "notes.txt", "browser_download_url": "https://github.com/e", "size": 5},
            ]
        });
        let asset = choose_windows_x64_installer(&release).unwrap();
        assert_eq!(
            (asset.name.as_str(), asset.url.as_str(), asset.size),
            ("USBip-0.9-x64-setup.exe", "https://github.com/c", 3)
        );

        let empty =
            json!({"tag_name": "v1", "assets": [{"name": "src.zip", "browser_download_url": "https://github.com/x"}]});
        let err = choose_windows_x64_installer(&empty).err().unwrap();
        assert!(err.starts_with("release v1 does not contain"), "{err}");
    }

    #[test]
    fn resolves_redirect_locations() {
        let base = "https://github.com/vadimgrn/usbip-win2/releases/latest";
        assert_eq!(
            resolve_location(base, "https://objects.githubusercontent.com/x"),
            "https://objects.githubusercontent.com/x"
        );
        assert_eq!(
            resolve_location(base, "/vadimgrn/usbip-win2/releases/tag/v1"),
            "https://github.com/vadimgrn/usbip-win2/releases/tag/v1"
        );
        assert_eq!(resolve_location(base, "//api.github.com/x"), "https://api.github.com/x");
        assert_eq!(resolve_location(base, "v1"), "https://github.com/vadimgrn/usbip-win2/releases/v1");
    }

    #[test]
    fn name_resolution_errors() {
        assert!(is_name_resolution_error(&DownloadError { message: "x".into(), name_resolution: true }));
        assert!(is_name_resolution_error(&DownloadError::new("lookup api.github.com: no such host")));
        assert!(!is_name_resolution_error(&DownloadError::new("HTTP 404 Not Found")));
    }
}
