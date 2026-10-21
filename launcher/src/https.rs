//! HTTPS downloads inside the process, for the automatic update (and the developer program's `update` command).
//!
//! Why not a helper program: 0.2.0 downloaded with Windows' `curl.exe`. A child process started by Rust's
//! `std::process::Command` inherits every inheritable handle of its parent, and inside the game those are many (the C
//! runtime opens files inheritable). The game crashed at its start on 2026-10-22 ("Internal error": the rename of
//! `shader_cache\pipelines.cache.tmp` failed "being used by another process") while curl ran with the game's files
//! open. So no child process is started from the game process at all (`tests/no_child_process.rs`).
//!
//! The download uses WinHTTP from the Windows system folder, loaded by full path and called through
//! `GetProcAddress`. That matters inside the game: our own module IS `winhttp.dll` (the proxy in the game folder), so
//! the system copy is loaded by its full path, exactly as `native/src/proxy.rs` does, and never found by name.
//!
//! The rules, the same as with curl: HTTPS only (port 443), the server certificate checked by Windows (no flag that
//! ignores a certificate error is ever set), TLS 1.2 or newer, a user agent, time limits, a size limit, and only the
//! hosts in [`ALLOWED_HOSTS`]. Redirects are not followed by WinHTTP: each one is checked here ([`redirect_target`])
//! and followed at most [`MAX_REDIRECTS`] times, to an allowed host only.

use std::time::Duration;

/// The only hosts a download may reach: GitHub's API, GitHub itself (release download addresses) and the hosts GitHub
/// redirects a release file to (`objects.githubusercontent.com`, since 2025 `release-assets.githubusercontent.com`).
pub const ALLOWED_HOSTS: &[&str] =
    &["api.github.com", "github.com", "objects.githubusercontent.com", "release-assets.githubusercontent.com"];

/// How many redirects one download may follow (GitHub uses one for a release file).
pub const MAX_REDIRECTS: u32 = 5;

/// The whole download, redirects included.
pub const TOTAL_TIME: Duration = Duration::from_secs(900);

/// The `User-Agent` of every request (GitHub's API refuses requests without one).
pub fn user_agent() -> String {
    format!("feverlink3-updater/{}", env!("CARGO_PKG_VERSION"))
}

/// An address split for WinHTTP: the host and the path with its query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub path: String,
}

/// Checks an address: `https://`, an allowed host, no user name or password, no port but 443, no control characters
/// or spaces. The fragment is dropped (it is never sent).
pub fn check_url(url: &str) -> Result<Target, String> {
    let rest = url.strip_prefix("https://").ok_or_else(|| format!("refusing a non-HTTPS address: {url}"))?;
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(format!("refusing an address with spaces or control characters: {url:?}"));
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    if authority.contains('@') {
        return Err(format!("refusing an address with a user name: {url}"));
    }
    let host = match authority.rsplit_once(':') {
        Some((h, "443")) => h,
        Some(_) => return Err(format!("refusing an address with another port: {url}")),
        None => authority,
    };
    let host = host.to_ascii_lowercase();
    if !ALLOWED_HOSTS.contains(&host.as_str()) {
        return Err(format!("refusing a host that is not GitHub's: {url}"));
    }
    let tail = tail.split('#').next().unwrap_or("");
    let path = if tail.starts_with('/') { tail.to_owned() } else { format!("/{tail}") };
    Ok(Target { host, path })
}

/// The address a redirect leads to: an absolute `Location` is checked like any address, one starting with `/` stays on
/// the same host. Anything else (another scheme, a relative path, `//host`) is refused.
pub fn redirect_target(from: &Target, location: &str) -> Result<Target, String> {
    let location = location.trim();
    if location.starts_with('/') && !location.starts_with("//") {
        return check_url(&format!("https://{}{location}", from.host));
    }
    check_url(location)
}

/// What one request returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Body(Vec<u8>),
    Redirect(String),
}

/// Downloads `url` (at most `max_bytes`), following redirects to allowed hosts only. `request` performs one GET of a
/// checked target (WinHTTP in the program; a fake in the tests).
pub fn get_with(
    url: &str,
    max_bytes: u64,
    request: &mut dyn FnMut(&Target, u64) -> Result<Answer, String>,
) -> Result<Vec<u8>, String> {
    let mut target = check_url(url)?;
    for _ in 0..=MAX_REDIRECTS {
        match request(&target, max_bytes)? {
            Answer::Body(b) if b.len() as u64 > max_bytes => {
                return Err(format!("{url} is larger than {max_bytes} bytes"));
            }
            Answer::Body(b) => return Ok(b),
            Answer::Redirect(location) => target = redirect_target(&target, &location)?,
        }
    }
    Err(format!("{url}: more than {MAX_REDIRECTS} redirects"))
}

/// Downloads `url` with the system's WinHTTP (see the module documentation).
pub fn get(url: &str, max_bytes: u64) -> Result<Vec<u8>, String> {
    let deadline = std::time::Instant::now() + TOTAL_TIME;
    get_with(url, max_bytes, &mut |t, max| imp::request(t, max, deadline))
}

#[cfg(windows)]
mod imp {
    use super::{user_agent, Answer, Target};
    use std::ffi::c_void;
    use std::ptr::null;
    use std::ptr::null_mut;
    use std::sync::OnceLock;
    use std::time::Instant;

    type H = *mut c_void;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetSystemDirectoryW(buffer: *mut u16, size: u32) -> u32;
        fn LoadLibraryW(name: *const u16) -> H;
        fn GetProcAddress(module: H, name: *const u8) -> *mut c_void;
        fn GetLastError() -> u32;
    }

    type Open = unsafe extern "system" fn(*const u16, u32, *const u16, *const u16, u32) -> H;
    type SetTimeouts = unsafe extern "system" fn(H, i32, i32, i32, i32) -> i32;
    type SetOption = unsafe extern "system" fn(H, u32, *const c_void, u32) -> i32;
    type Connect = unsafe extern "system" fn(H, *const u16, u16, u32) -> H;
    type OpenRequest =
        unsafe extern "system" fn(H, *const u16, *const u16, *const u16, *const u16, *const *const u16, u32) -> H;
    type SendRequest = unsafe extern "system" fn(H, *const u16, u32, *const c_void, u32, u32, usize) -> i32;
    type ReceiveResponse = unsafe extern "system" fn(H, *mut c_void) -> i32;
    type QueryHeaders = unsafe extern "system" fn(H, u32, *const u16, *mut c_void, *mut u32, *mut u32) -> i32;
    type ReadData = unsafe extern "system" fn(H, *mut c_void, u32, *mut u32) -> i32;
    type CloseHandle = unsafe extern "system" fn(H) -> i32;

    /// The system WinHTTP's functions.
    struct Api {
        open: Open,
        set_timeouts: SetTimeouts,
        set_option: SetOption,
        connect: Connect,
        open_request: OpenRequest,
        send_request: SendRequest,
        receive_response: ReceiveResponse,
        query_headers: QueryHeaders,
        read_data: ReadData,
        close: CloseHandle,
    }

    const ACCESS_TYPE_DEFAULT_PROXY: u32 = 0;
    const ACCESS_TYPE_AUTOMATIC_PROXY: u32 = 4;
    const FLAG_SECURE: u32 = 0x0080_0000;
    const OPTION_SECURE_PROTOCOLS: u32 = 84;
    const OPTION_REDIRECT_POLICY: u32 = 88;
    const REDIRECT_POLICY_NEVER: u32 = 0;
    const PROTOCOL_TLS1_2: u32 = 0x0000_0800;
    const PROTOCOL_TLS1_3: u32 = 0x0000_2000;
    const QUERY_STATUS_CODE: u32 = 19;
    const QUERY_LOCATION: u32 = 33;
    const QUERY_FLAG_NUMBER: u32 = 0x2000_0000;
    const HTTPS_PORT: u16 = 443;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// `<system folder>\winhttp.dll`, loaded by full path: inside the game a module named `winhttp.dll` is already
    /// loaded (ours), and only the full path reaches the system's. Loaded once and never freed.
    fn api() -> Result<&'static Api, String> {
        static API: OnceLock<Option<Api>> = OnceLock::new();
        API.get_or_init(load).as_ref().ok_or_else(|| "the system's winhttp.dll could not be loaded".to_owned())
    }

    fn load() -> Option<Api> {
        let mut dir = [0u16; 300];
        // SAFETY: the buffer holds 300 wide characters; at most 260 are asked for, the length is returned.
        let n = unsafe { GetSystemDirectoryW(dir.as_mut_ptr(), 260) } as usize;
        if n == 0 || n >= 260 {
            return None;
        }
        let mut path: Vec<u16> = dir[..n].to_vec();
        path.extend("\\winhttp.dll\0".encode_utf16());
        // SAFETY: `path` is a NUL-terminated wide string, a full path.
        let m = unsafe { LoadLibraryW(path.as_ptr()) };
        if m.is_null() {
            return None;
        }
        macro_rules! proc {
            ($ty:ty, $name:literal) => {{
                // SAFETY: `m` is a loaded module, the name is NUL-terminated ASCII; the pointer is cast to the
                // function type the WinHTTP documentation gives for this name.
                let p = unsafe { GetProcAddress(m, concat!($name, "\0").as_ptr()) };
                if p.is_null() {
                    return None;
                }
                unsafe { std::mem::transmute::<*mut c_void, $ty>(p) }
            }};
        }
        Some(Api {
            open: proc!(Open, "WinHttpOpen"),
            set_timeouts: proc!(SetTimeouts, "WinHttpSetTimeouts"),
            set_option: proc!(SetOption, "WinHttpSetOption"),
            connect: proc!(Connect, "WinHttpConnect"),
            open_request: proc!(OpenRequest, "WinHttpOpenRequest"),
            send_request: proc!(SendRequest, "WinHttpSendRequest"),
            receive_response: proc!(ReceiveResponse, "WinHttpReceiveResponse"),
            query_headers: proc!(QueryHeaders, "WinHttpQueryHeaders"),
            read_data: proc!(ReadData, "WinHttpReadData"),
            close: proc!(CloseHandle, "WinHttpCloseHandle"),
        })
    }

    /// A WinHTTP handle, closed when dropped.
    struct Handle(&'static Api, H);

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: a handle WinHTTP returned and nobody closed yet.
            unsafe { (self.0.close)(self.1) };
        }
    }

    fn handle(api: &'static Api, h: H, what: &str) -> Result<Handle, String> {
        if h.is_null() {
            // SAFETY: plain Win32 call.
            Err(format!("{what} failed (WinHTTP error {})", unsafe { GetLastError() }))
        } else {
            Ok(Handle(api, h))
        }
    }

    fn fail(what: &str) -> String {
        // SAFETY: plain Win32 call.
        format!("{what} failed (WinHTTP error {})", unsafe { GetLastError() })
    }

    /// One GET of a checked target. Redirects come back as [`Answer::Redirect`]; any other status but 200 is an error.
    pub fn request(t: &Target, max_bytes: u64, deadline: Instant) -> Result<Answer, String> {
        let api = api()?;
        let agent = wide(&user_agent());
        // SAFETY (all calls below): WinHTTP calls with handles WinHTTP returned (kept alive by `Handle`), wide strings
        // that are NUL-terminated and live across the call, and buffers whose sizes are passed with them.
        unsafe {
            let mut session = (api.open)(agent.as_ptr(), ACCESS_TYPE_AUTOMATIC_PROXY, null(), null(), 0);
            if session.is_null() {
                // before Windows 8.1 the automatic proxy type does not exist
                session = (api.open)(agent.as_ptr(), ACCESS_TYPE_DEFAULT_PROXY, null(), null(), 0);
            }
            let session = handle(api, session, "WinHttpOpen")?;
            (api.set_timeouts)(session.1, 30_000, 30_000, 30_000, 60_000);
            let tls = PROTOCOL_TLS1_2 | PROTOCOL_TLS1_3;
            let tls12 = PROTOCOL_TLS1_2;
            let set = |v: &u32| (api.set_option)(session.1, OPTION_SECURE_PROTOCOLS, (v as *const u32).cast(), 4);
            // TLS 1.3 is known to WinHTTP only from Windows 11 / Server 2022 on
            if set(&tls) == 0 && set(&tls12) == 0 {
                return Err(fail("TLS 1.2 setting"));
            }
            let never = REDIRECT_POLICY_NEVER;
            if (api.set_option)(session.1, OPTION_REDIRECT_POLICY, (&never as *const u32).cast(), 4) == 0 {
                return Err(fail("the redirect setting"));
            }
            let host = wide(&t.host);
            let connection = handle(api, (api.connect)(session.1, host.as_ptr(), HTTPS_PORT, 0), "WinHttpConnect")?;
            let (verb, path) = (wide("GET"), wide(&t.path));
            let req =
                (api.open_request)(connection.1, verb.as_ptr(), path.as_ptr(), null(), null(), null(), FLAG_SECURE);
            let req = handle(api, req, "WinHttpOpenRequest")?;
            if (api.send_request)(req.1, null(), 0, null(), 0, 0, 0) == 0 {
                return Err(fail(&format!("the request to {}", t.host)));
            }
            if (api.receive_response)(req.1, null_mut()) == 0 {
                return Err(fail(&format!("the answer of {}", t.host)));
            }
            let mut status: u32 = 0;
            let mut len: u32 = 4;
            let flags = QUERY_STATUS_CODE | QUERY_FLAG_NUMBER;
            if (api.query_headers)(req.1, flags, null(), (&mut status as *mut u32).cast(), &mut len, null_mut()) == 0 {
                return Err(fail("reading the status"));
            }
            match status {
                200 => {}
                301 | 302 | 303 | 307 | 308 => {
                    let mut buf = vec![0u16; 8192];
                    let mut bytes = (buf.len() * 2) as u32;
                    let ok = (api.query_headers)(
                        req.1,
                        QUERY_LOCATION,
                        null(),
                        buf.as_mut_ptr().cast(),
                        &mut bytes,
                        null_mut(),
                    );
                    if ok == 0 {
                        return Err(fail("reading the redirect"));
                    }
                    buf.truncate(bytes as usize / 2);
                    return Ok(Answer::Redirect(String::from_utf16_lossy(&buf)));
                }
                other => {
                    return Err(format!("{}{} answered HTTP {other}", t.host, t.path.split('?').next().unwrap_or("")))
                }
            }
            let mut body = Vec::new();
            let mut chunk = vec![0u8; 64 << 10];
            loop {
                if Instant::now() > deadline {
                    return Err(format!("the download from {} took too long", t.host));
                }
                let mut read: u32 = 0;
                if (api.read_data)(req.1, chunk.as_mut_ptr().cast(), chunk.len() as u32, &mut read) == 0 {
                    return Err(fail(&format!("reading from {}", t.host)));
                }
                if read == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..read as usize]);
                if body.len() as u64 > max_bytes {
                    return Err(format!("the file from {} is larger than {max_bytes} bytes", t.host));
                }
            }
            Ok(Answer::Body(body))
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::{Answer, Target};

    pub fn request(_t: &Target, _max_bytes: u64, _deadline: std::time::Instant) -> Result<Answer, String> {
        Err("downloads need Windows".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_https_to_githubs_hosts_on_port_443() {
        let ok = check_url("https://api.github.com/repos/AZalshehri7/FeverLink3/releases/latest").unwrap();
        assert_eq!(
            ok,
            Target { host: "api.github.com".into(), path: "/repos/AZalshehri7/FeverLink3/releases/latest".into() }
        );
        let q = check_url("https://Objects.GitHubUserContent.com:443/x?sig=a%2Fb#frag").unwrap();
        assert_eq!(q, Target { host: "objects.githubusercontent.com".into(), path: "/x?sig=a%2Fb".into() });
        assert_eq!(check_url("https://github.com").unwrap().path, "/");
        assert!(check_url("https://release-assets.githubusercontent.com/a").is_ok());
        for bad in [
            "http://github.com/x",
            "HTTP://github.com/x",
            "ftp://github.com/x",
            "https://evil.example/x",
            "https://github.com.evil.example/x",
            "https://evilgithub.com/x",
            "https://github.com@evil.example/x",
            "https://user:pw@github.com/x",
            "https://github.com:8443/x",
            "https://github.com:80/x",
            "https://github.com/a b",
            "https://github.com/a\r\nHost: evil",
            "github.com/x",
            "",
        ] {
            assert!(check_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn redirects_stay_on_allowed_hosts() {
        let from = check_url("https://github.com/AZalshehri7/FeverLink3/releases/download/v0.2.1/a.zip").unwrap();
        let to = redirect_target(&from, "https://objects.githubusercontent.com/github-production/abc?X=1").unwrap();
        assert_eq!(to.host, "objects.githubusercontent.com");
        assert_eq!(
            redirect_target(&from, "/other/path").unwrap(),
            Target { host: "github.com".into(), path: "/other/path".into() }
        );
        for bad in [
            "http://objects.githubusercontent.com/x",
            "https://evil.example/x",
            "//evil.example/x",
            "x/y",
            "file:///C:/x",
        ] {
            assert!(redirect_target(&from, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn get_follows_good_redirects_refuses_bad_ones_and_limits_size_and_count() {
        let start = "https://github.com/AZalshehri7/FeverLink3/releases/download/v0.2.1/a.zip";
        let mut seen = Vec::new();
        let mut gh = |t: &Target, _max: u64| {
            seen.push(t.host.clone());
            Ok(match t.host.as_str() {
                "github.com" => Answer::Redirect("https://objects.githubusercontent.com/blob".into()),
                _ => Answer::Body(b"zip".to_vec()),
            })
        };
        assert_eq!(get_with(start, 10, &mut gh).unwrap(), b"zip");
        assert_eq!(seen, ["github.com", "objects.githubusercontent.com"]);

        let mut to_evil = |_: &Target, _: u64| Ok(Answer::Redirect("https://evil.example/zip".into()));
        assert!(get_with(start, 10, &mut to_evil).is_err());
        let mut to_http = |_: &Target, _: u64| Ok(Answer::Redirect("http://objects.githubusercontent.com/zip".into()));
        assert!(get_with(start, 10, &mut to_http).is_err());

        let mut n = 0;
        let mut endless = |_: &Target, _: u64| {
            n += 1;
            Ok(Answer::Redirect("/again".into()))
        };
        assert!(get_with(start, 10, &mut endless).is_err());
        assert_eq!(n, MAX_REDIRECTS + 1);

        let mut big = |_: &Target, _: u64| Ok(Answer::Body(vec![0; 11]));
        assert!(get_with(start, 10, &mut big).is_err());

        // a refused first address never reaches the network
        let mut never = |_: &Target, _: u64| -> Result<Answer, String> { panic!("no request expected") };
        assert!(get_with("http://github.com/x", 10, &mut never).is_err());
        assert!(get("https://evil.example/x", 10).is_err());
    }

    /// The real GitHub over the system's WinHTTP (network; run by hand: `cargo test -p feverlink3_launcher https --
    /// --include-ignored`): the API answers, a release file follows GitHub's redirect to its file host (another
    /// project's release: FeverLink3 may have none listed), the size limit holds.
    #[test]
    #[ignore = "needs the network"]
    fn real_github() {
        let repo = get("https://api.github.com/repos/AZalshehri7/FeverLink3", 2 << 20).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&repo).unwrap();
        assert_eq!(json["name"], "FeverLink3");
        let file = "https://github.com/BurntSushi/ripgrep/releases/download/14.1.0/ripgrep-14.1.0-x86_64-pc-windows-msvc.zip.sha256";
        let sha = String::from_utf8(get(file, 4 << 10).unwrap()).unwrap();
        assert!(sha.contains("ripgrep-14.1.0-x86_64-pc-windows-msvc.zip"), "{sha}");
        assert!(get(file, 10).is_err(), "the size limit holds");
    }
}
