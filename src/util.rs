use std::io::{BufRead, Write};

use windows::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};
use windows::Win32::System::Console::SetConsoleOutputCP;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

pub fn init_console() {
    unsafe {
        let _ = SetConsoleOutputCP(65001);
    }
}

/// Physical-pixel coordinates everywhere, so capture and SendInput agree on high-DPI screens.
pub fn init_dpi() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// Joins the process-wide multithreaded COM apartment from the main thread, so it lives
/// as long as the process. windows-rs caches WinRT factories globally; if the apartment is
/// torn down when a capture thread exits, the next capture crashes on a stale factory.
pub fn init_com() {
    unsafe {
        let _ = windows::Win32::System::WinRT::RoInitialize(windows::Win32::System::WinRT::RO_INIT_MULTITHREADED);
    }
}

/// Log file next to the exe; started over once it passes this size.
const LOG_MAX: u64 = 5 << 20;
static LOG_FILE: std::sync::Mutex<Option<std::fs::File>> = std::sync::Mutex::new(None);

fn log_path() -> std::path::PathBuf {
    std::env::current_exe()
        .unwrap_or_else(|_| "ezcontrol.exe".into())
        .with_file_name("ezcontrol.log")
}

/// Prints to the console and appends to ezcontrol.log.
pub fn log(msg: &str) {
    let t = unsafe { GetLocalTime() };
    let line = format!(
        "[{:04}-{:02}-{:02} {:02}:{:02}:{:02}] {msg}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    );
    println!("{line}");
    let mut f = LOG_FILE.lock().unwrap();
    if f.as_ref().is_none_or(|f| f.metadata().is_ok_and(|m| m.len() > LOG_MAX)) {
        let path = log_path();
        let truncate = std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_MAX);
        *f = std::fs::OpenOptions::new()
            .create(true)
            .append(!truncate)
            .write(true)
            .truncate(truncate)
            .open(path)
            .ok();
    }
    if let Some(f) = f.as_mut() {
        let _ = writeln!(f, "{line}");
    }
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    let st = unsafe { BCryptGenRandom(None, &mut buf, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    assert!(st.is_ok(), "BCryptGenRandom failed");
    buf
}

pub fn random_password(len: usize) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnpqrstuvwxyz23456789";
    let bytes = random_bytes::<64>();
    bytes[..len]
        .iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect()
}

pub fn prompt_password() -> String {
    print!("请输入新密码: ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).expect("读取输入失败");
    line.trim_end_matches(['\r', '\n']).to_string()
}

pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
