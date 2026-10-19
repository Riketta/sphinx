//! Win32 window discovery + process-level helpers shared by the pixel
//! reader and the KBM backend (§3.7: every configured point resolves
//! against the live game window rect at use time).

use tracing::warn;
use windows::Win32::{
    Foundation::{
        CloseHandle,
        BOOL,
        HANDLE,
        HWND,
        LPARAM,
        POINT,
        RECT,
    },
    Graphics::Gdi::ClientToScreen,
    Security::{
        GetTokenInformation,
        TokenElevation,
        TOKEN_ELEVATION,
        TOKEN_QUERY,
    },
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot,
            Process32FirstW,
            Process32NextW,
            PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        },
        Threading::{
            GetCurrentProcess,
            OpenProcess,
            OpenProcessToken,
            PROCESS_QUERY_LIMITED_INFORMATION,
        },
    },
    UI::{
        HiDpi::{
            GetDpiForSystem,
            SetProcessDpiAwarenessContext,
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        },
        WindowsAndMessaging::{
            EnumWindows,
            GetClientRect,
            GetForegroundWindow,
            GetSystemMetrics,
            GetWindowTextLengthW,
            GetWindowTextW,
            GetWindowThreadProcessId,
            IsWindow,
            IsWindowVisible,
            SetForegroundWindow,
            SM_CXSCREEN,
            SM_CYSCREEN,
        },
    },
};

/// Client process image names sphinx looks for (case-insensitive). `WowB.exe`
/// is this fork's client; the others cover common `WoW` binary names.
pub const DEFAULT_PROCESS_NAMES: &[&str] = &["wowb.exe", "wow.exe", "wowclassic.exe", "wow-64.exe"];

/// A discovered game window: everything later calls need to translate
/// client coordinates or check the foreground policy.
#[derive(Clone, Debug)]
pub struct GameWindowInfo {
    /// Win32 window handle of the game's top-level window.
    pub hwnd: HWND,
    /// Owning process id.
    pub pid: u32,
    /// Window title (empty string if none).
    pub title: String,
    /// Client area size `(width, height)`.
    pub client: (i32, i32),
}

impl GameWindowInfo {
    /// Translates a client-area point to physical screen coordinates via
    /// the live window position (§3.7). `None` if the window vanished.
    pub fn client_to_screen(&self, x: i32, y: i32) -> Option<(i32, i32)> {
        let mut p = POINT { x, y };
        // SAFETY: hwnd was a valid top-level window moments ago; POINT is a
        // local out-param; ClientToScreen only writes to it.
        let ok = unsafe { ClientToScreen(self.hwnd, &raw mut p) };
        ok.as_bool().then_some((p.x, p.y))
    }
}

/// Finds the game window by process image name. Re-run per use: handles
/// client restarts and never caches a dead handle.
#[derive(Clone)]
pub struct GameWindow {
    process_names: Vec<String>,
    title_filter: Option<String>,
}

impl GameWindow {
    /// Configures discovery with case-insensitive image names (e.g.
    /// `wow.exe`) and an optional title-substring filter. See
    /// [`DEFAULT_PROCESS_NAMES`].
    pub fn new(process_names: &[String], title_filter: Option<&str>) -> Self {
        Self {
            process_names: process_names.to_vec(),
            title_filter: title_filter.map(str::to_string),
        }
    }

    /// Finds the game's main window: the visible top-level window with the
    /// largest client area among all processes matching the name list (and
    /// the title filter, when configured).
    pub fn find(&self) -> Option<GameWindowInfo> {
        let pids = find_pids(&self.process_names);
        if pids.is_empty() {
            return None;
        }
        let hwnd = largest_visible_window(&pids, self.title_filter.as_deref())?;
        let mut pid = 0;
        // SAFETY: out-param is a local; hwnd came from EnumWindows above.
        unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut pid)) };
        Some(GameWindowInfo {
            hwnd,
            pid,
            title: window_title(hwnd),
            client: client_size(hwnd).unwrap_or((0, 0)),
        })
    }
}

/// Marks the process per-monitor DPI aware so all coordinates are physical
/// pixels. Call once at startup, before any pixel or input work.
pub fn ensure_dpi_awareness() {
    // SAFETY: documented one-shot process attribute; constant context value.
    if unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }.is_err()
    {
        warn!("SetProcessDpiAwarenessContext failed (already set?) - coordinates may be scaled");
    }
}

/// System DPI (96 = 100%), for `doctor` output.
pub fn system_dpi() -> u32 {
    // SAFETY: trivial process-wide query.
    unsafe { GetDpiForSystem() }
}

/// Primary monitor size in physical pixels (`(width, height)`).
pub fn screen_size() -> (i32, i32) {
    // SAFETY: trivial metrics query with enum constants.
    unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) }
}

/// Handle of the foreground window, or `None` if there is none.
pub fn foreground_hwnd() -> Option<HWND> {
    // SAFETY: trivial query.
    let hwnd = unsafe { GetForegroundWindow() };
    (!hwnd.is_invalid()).then_some(hwnd)
}

/// Whether `hwnd` is still a live window.
pub fn window_alive(hwnd: HWND) -> bool {
    // SAFETY: read-only validity query.
    unsafe { IsWindow(hwnd) }.as_bool()
}

/// Whether two handles denote the same window.
pub fn same_window(a: HWND, b: HWND) -> bool {
    a == b
}

/// Best-effort focus of the game window; Windows may refuse when the caller
/// is not the foreground process. Returns whether it now is foreground.
pub fn try_focus(hwnd: HWND) -> bool {
    // SAFETY: user-opt-in focus attempt on the discovered game window.
    unsafe {
        let _ = SetForegroundWindow(hwnd);
    }
    foreground_hwnd().is_some_and(|fg| fg == hwnd)
}

/// Window title of `hwnd` (empty string when it has none).
pub fn window_title(hwnd: HWND) -> String {
    // SAFETY: length query + bounded copy into a locally sized buffer.
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    let len = usize::try_from(len).unwrap_or(0);
    if len == 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len + 1];
    // SAFETY: buf is len+1 wide chars, large enough for len chars + NUL.
    let copied = unsafe { GetWindowTextW(hwnd, &mut buf) };
    let copied = usize::try_from(copied).unwrap_or(0);
    buf.truncate(copied);
    String::from_utf16_lossy(&buf)
}

/// Image name (e.g. `wow.exe`) of the process owning `hwnd`, if resolvable.
pub fn window_process_image(hwnd: HWND) -> Option<String> {
    let mut pid = 0;
    // SAFETY: out-param is a local.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut pid)) };
    if pid == 0 {
        return None;
    }
    process_image_name(pid)
}

/// Image name of process `pid`, if it is still running.
pub fn process_image_name(pid: u32) -> Option<String> {
    // SAFETY: snapshot created/closed here; entry dwSize set before use.
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.ok()?;
    let matches: Vec<String> = with_process_entries(snap, &mut |entry| {
        if entry.th32ProcessID == pid {
            Some(exe_name(&entry.szExeFile))
        } else {
            None
        }
    });
    // SAFETY: closing the snapshot handle we own.
    unsafe {
        let _ = CloseHandle(snap);
    }
    matches.into_iter().next()
}

fn find_pids(process_names: &[String]) -> Vec<u32> {
    // SAFETY: snapshot created/closed here; entry dwSize set before use.
    let snap = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) } {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "CreateToolhelp32Snapshot failed");
            return Vec::new();
        }
    };
    let pids: Vec<u32> = with_process_entries(snap, &mut |entry| {
        let name = exe_name(&entry.szExeFile);
        process_names
            .iter()
            .find(|want| want.eq_ignore_ascii_case(&name))
            .map(|_| entry.th32ProcessID)
    });
    // SAFETY: closing the snapshot handle we own.
    unsafe {
        let _ = CloseHandle(snap);
    }
    pids
}

/// Walks the process snapshot, invoking `visit` per entry and collecting its
/// `Some(..)` results. `snap` must come from `CreateToolhelp32Snapshot`.
fn with_process_entries<T>(
    snap: HANDLE,
    visit: &mut dyn FnMut(&PROCESSENTRY32W) -> Option<T>,
) -> Vec<T> {
    let mut entries = Vec::new();
    let mut entry = PROCESSENTRY32W {
        dwSize: u32::try_from(std::mem::size_of::<PROCESSENTRY32W>()).unwrap_or(0),
        ..PROCESSENTRY32W::default()
    };
    if entry.dwSize == 0 {
        return entries;
    }
    // SAFETY: entry.dwSize is set to sizeof(PROCESSENTRY32W) per the Win32
    // contract; snap is a valid snapshot handle.
    if unsafe { Process32FirstW(snap, &raw mut entry) }.is_err() {
        return entries;
    }
    loop {
        if let Some(hit) = visit(&entry) {
            entries.push(hit);
        }
        // SAFETY: same entry/handle as the First call; ends when false.
        if unsafe { Process32NextW(snap, &raw mut entry) }.is_err() {
            break;
        }
    }
    entries
}

/// NUL-trimmed, lowercased copy of a fixed-wide-char process image name.
fn exe_name(raw: &[u16; 260]) -> String {
    let len = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    let name: Vec<u16> = raw.iter().take(len).copied().collect();
    String::from_utf16_lossy(&name).to_lowercase()
}

struct EnumCtx<'a> {
    pids: &'a [u32],
    /// Lowercased title-substring filter; `None` matches everything.
    title: Option<String>,
    hits: Vec<(HWND, i64)>,
}

// SAFETY: lparam carries a valid &mut EnumCtx boxed on the stack by
// `largest_visible_window`, which outlives the enumeration call.
unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: lparam is the pointer `largest_visible_window` passed below; it
    // points at an EnumCtx that outlives the enumeration call.
    let ctx = unsafe { &mut *(lparam.0 as *mut EnumCtx<'_>) };
    let mut pid = 0;
    // SAFETY: out-param is a local.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut pid)) };
    if !ctx.pids.contains(&pid) || pid == 0 {
        return true.into();
    }
    // SAFETY: read-only visibility query on an enumerated window.
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return true.into();
    }
    if let Some(want) = &ctx.title {
        // Candidate windows are few: a title read per hit is cheap.
        if !window_title(hwnd).to_lowercase().contains(want.as_str()) {
            return true.into();
        }
    }
    if let Some((w, h)) = client_size(hwnd) {
        let area = i64::from(w) * i64::from(h);
        if area > 0 {
            ctx.hits.push((hwnd, area));
        }
    }
    true.into()
}

fn largest_visible_window(pids: &[u32], title_filter: Option<&str>) -> Option<HWND> {
    let mut ctx = EnumCtx { pids, title: title_filter.map(str::to_lowercase), hits: Vec::new() };
    let lparam = LPARAM(&raw mut ctx as isize);
    // SAFETY: ctx outlives the call; the callback only appends to ctx.hits.
    unsafe {
        let _ = EnumWindows(Some(enum_proc), lparam);
    }
    ctx.hits.sort_by_key(|hit| std::cmp::Reverse(hit.1));
    ctx.hits.first().map(|hit| hit.0)
}

fn client_size(hwnd: HWND) -> Option<(i32, i32)> {
    let mut rect = RECT::default();
    // SAFETY: out-param is a local RECT.
    unsafe { GetClientRect(hwnd, &raw mut rect) }.ok()?;
    Some((rect.right - rect.left, rect.bottom - rect.top))
}

/// Whether the process `pid` runs elevated (admin). `None` when the token
/// cannot be queried (process gone, or access refused).
pub fn process_elevated(pid: u32) -> Option<bool> {
    // SAFETY: read-only query with limited rights on a live pid.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let elevated = token_elevated(process);
    // SAFETY: closing the process handle we opened above.
    unsafe {
        let _ = CloseHandle(process);
    }
    elevated
}

/// Whether sphinx itself runs elevated.
pub fn self_elevated() -> Option<bool> {
    // SAFETY: GetCurrentProcess returns a pseudo handle that must not (and
    // cannot meaningfully) be closed.
    token_elevated(unsafe { GetCurrentProcess() })
}

/// Queries `TokenElevation` for a process (or pseudo-handle) token.
fn token_elevated(process: HANDLE) -> Option<bool> {
    let mut token = HANDLE::default();
    // SAFETY: out-param is a local; the input is a process/pseudo handle.
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) }.ok()?;
    let mut out = TOKEN_ELEVATION::default();
    let mut returned = 0u32;
    // SAFETY: out buffer is sized exactly for TOKEN_ELEVATION; token is
    // closed below on every path.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some((&raw mut out).cast()),
            u32::try_from(std::mem::size_of::<TOKEN_ELEVATION>()).unwrap_or(0),
            &raw mut returned,
        )
    }
    .is_ok();
    // SAFETY: closing the token handle we opened above.
    unsafe {
        let _ = CloseHandle(token);
    }
    ok.then_some(out.TokenIsElevated != 0)
}
