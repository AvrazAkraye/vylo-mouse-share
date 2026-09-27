//! Windows: optionally run Vylo as administrator ("Control admin windows").
//!
//! Windows does not let a normal app inject input into the window of an app
//! running as administrator (UIPI). Without this, the peer's mouse can move
//! over an installer, a VPN pop-up or Task Manager but cannot click it - only
//! this machine's own mouse can. Running Vylo elevated lifts that.
//!
//! A Run-key login item cannot start an elevated app: Windows silently skips
//! it. So in admin mode the login start is a scheduled task set to "run with
//! highest privileges", which starts elevated with no UAC prompt. A manual
//! start from the Start menu asks once through UAC and hands over to the
//! elevated copy.
//!
//! UAC prompts and the lock screen stay out of reach even elevated - they
//! run on a separate secure desktop.

use std::ffi::c_void;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use windows::core::{HSTRING, PCWSTR, w};
use windows::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_SYNCHRONIZE, WaitForSingleObject,
};
use windows::Win32::UI::Shell::{SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Name of the login task in Task Scheduler.
const TASK_NAME: &str = "Vylo Mouse Share";
/// `--elevated-handoff <pid>`: this process was started by [`relaunch_elevated`]
/// from process `pid`, which is about to exit.
const HANDOFF_FLAG: &str = "--elevated-handoff";
/// Keeps `schtasks` from flashing a console window.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Whether this process runs with an elevated (administrator) token.
pub fn is_elevated() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut TOKEN_ELEVATION as *mut c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// Start an elevated copy of Vylo through UAC with this launch's arguments.
///
/// `Ok(false)` when the user declined the prompt. On `Ok(true)` the caller
/// must exit promptly: the new copy waits for it before starting up, so that
/// the single-instance lock and the service's ports are free.
pub fn relaunch_elevated() -> Result<bool, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut params = format!("{HANDOFF_FLAG} {}", std::process::id());
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == HANDOFF_FLAG {
            args.next(); // and its pid
            continue;
        }
        params.push(' ');
        params.push_str(&quote(&arg));
    }
    let file = HSTRING::from(exe.as_os_str());
    let params = HSTRING::from(params);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    match unsafe { ShellExecuteExW(&mut info) } {
        Ok(()) => Ok(true),
        Err(e) if e.code() == ERROR_CANCELLED.to_hresult() => Ok(false),
        Err(e) => Err(e.message()),
    }
}

/// In a copy started by [`relaunch_elevated`]: wait until the copy that
/// started us has exited. Returns immediately for any other launch.
pub fn wait_for_handoff() {
    let mut args = std::env::args();
    let Some(pid) = args
        .by_ref()
        .skip_while(|a| a != HANDOFF_FLAG)
        .nth(1)
        .and_then(|pid| pid.parse::<u32>().ok())
    else {
        return;
    };
    unsafe {
        // gone already (or not ours to wait on): nothing to wait for
        let Ok(process) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) else {
            return;
        };
        let _ = WaitForSingleObject(process, 15_000);
        let _ = CloseHandle(process);
    }
}

/// Whether a Vylo is already running (its service answers on the IPC port).
/// A second launch should then just reveal it, not ask for elevation.
pub fn already_running() -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 5252));
    std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

fn schtasks(args: &[&str]) -> Result<(), String> {
    let out = Command::new("schtasks")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Whether the elevated login task exists.
pub fn task_exists() -> bool {
    schtasks(&["/Query", "/TN", TASK_NAME]).is_ok()
}

/// Create (or replace) the login task that starts Vylo elevated, in the
/// background. Needs an elevated process.
pub fn create_task() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let user = match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
        (Ok(domain), Ok(name)) => format!("{domain}\\{name}"),
        (_, Ok(name)) => name,
        _ => return Err("could not determine the current user".into()),
    };
    // Written as XML because the schtasks flags can't turn off the defaults
    // that break a long-running app: no start on battery, stop on battery,
    // killed after 72 hours, and below-normal priority (7) - too slow for
    // the input hooks.
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Starts Vylo Mouse Share as administrator when you sign in, so the shared mouse can use apps that run as administrator.</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>4</Priority>
    <Enabled>true</Enabled>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
      <Arguments>{hidden}</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        user = xml_escape(&user),
        exe = xml_escape(&exe.to_string_lossy()),
        hidden = crate::HIDDEN_FLAG,
    );
    // schtasks wants the file in UTF-16 when it says so
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
    let path: PathBuf = std::env::temp_dir().join("vylo-login-task.xml");
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    let result = schtasks(&[
        "/Create",
        "/F",
        "/TN",
        TASK_NAME,
        "/XML",
        &path.to_string_lossy(),
    ]);
    let _ = std::fs::remove_file(&path);
    result
}

/// Remove the login task; fine if it does not exist.
pub fn delete_task() -> Result<(), String> {
    if !task_exists() {
        return Ok(());
    }
    schtasks(&["/Delete", "/F", "/TN", TASK_NAME])
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Quote one argument for a Windows command line.
fn quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    format!("\"{}\"", arg.replace('"', "\\\""))
}
