//! Keyboard input-language monitoring and application.
//!
//! Vylo forwards physical key positions (scancodes), so the characters
//! produced when you type onto the peer depend on the peer's active
//! keyboard language. This module keeps the two machines' input
//! languages in sync: one thread polls the local input language, and
//! when it changes the new language is sent to the peer, which switches
//! to the matching layout. Languages are normalized to ISO 639-1 codes
//! (`en`, `ar`, `fr`, ...) so macOS input-source ids and Windows layout
//! ids map to a common wire value.
//!
//! Echo prevention: the language most recently applied from the peer is
//! remembered; when the poll observes that same language it is absorbed
//! rather than broadcast back, so no layout ping-pong can form.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, RecvTimeoutError, Sender, channel},
    },
    time::Duration,
};
use tokio::sync::mpsc::UnboundedSender;

const POLL_INTERVAL: Duration = Duration::from_millis(400);

pub(crate) struct KeyboardMonitor {
    apply_tx: Sender<String>,
}

impl KeyboardMonitor {
    /// `change_tx`: local language changes forwarded to the actor.
    /// `enabled`: the `sync_keyboard_layout` gate, read on every poll.
    pub(crate) fn new(change_tx: UnboundedSender<String>, enabled: Arc<AtomicBool>) -> Self {
        let (apply_tx, apply_rx) = channel();
        std::thread::Builder::new()
            .name("vylo-keyboard".into())
            .spawn(move || monitor(apply_rx, change_tx, enabled))
            .expect("failed to spawn keyboard thread");
        Self { apply_tx }
    }

    /// apply a remote language locally (called from the actor on receipt)
    pub(crate) fn apply(&self, lang: String) {
        let _ = self.apply_tx.send(lang);
    }
}

fn monitor(
    apply_rx: Receiver<String>,
    change_tx: UnboundedSender<String>,
    enabled: Arc<AtomicBool>,
) {
    let mut last_seen: Option<String> = current_language();
    // the language most recently applied from the peer; the next poll
    // that observes it is treated as an echo, not a local change
    let mut last_applied: Option<String> = None;

    loop {
        // apply remote languages, blocking up to one poll interval
        match apply_rx.recv_timeout(POLL_INTERVAL) {
            Ok(lang) => {
                apply_remote(&lang, &mut last_seen, &mut last_applied);
                while let Ok(lang) = apply_rx.try_recv() {
                    apply_remote(&lang, &mut last_seen, &mut last_applied);
                }
                continue;
            }
            Err(RecvTimeoutError::Timeout) => (),
            Err(RecvTimeoutError::Disconnected) => return,
        }

        if !enabled.load(Ordering::SeqCst) {
            continue;
        }

        if let Some(lang) = current_language() {
            if last_seen.as_deref() != Some(lang.as_str()) {
                last_seen = Some(lang.clone());
                if last_applied.as_deref() == Some(lang.as_str()) {
                    last_applied = None; // absorb the echo of a just-applied layout
                } else {
                    let _ = change_tx.send(lang);
                }
            }
        }
    }
}

fn apply_remote(lang: &str, last_seen: &mut Option<String>, last_applied: &mut Option<String>) {
    if current_language().as_deref() == Some(lang) {
        return; // already there
    }
    if select_language(lang) {
        *last_applied = Some(lang.to_string());
        *last_seen = current_language().or_else(|| Some(lang.to_string()));
    } else {
        log::warn!("could not switch keyboard language to '{lang}' (layout not installed?)");
    }
}

/* --------- per-OS implementations; always compiles via `dummy` --------- */

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub(crate) use dummy::{current_language, select_language};
#[cfg(target_os = "macos")]
pub(crate) use macos::{current_language, select_language};
#[cfg(target_os = "windows")]
pub(crate) use windows_impl::{current_language, select_language};

/// `en-US`, `zh-Hans` -> `en`, `zh`
#[allow(dead_code)]
fn primary_code(tag: &str) -> String {
    tag.split(['-', '_'])
        .next()
        .unwrap_or(tag)
        .to_ascii_lowercase()
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod dummy {
    pub(crate) fn current_language() -> Option<String> {
        None
    }
    pub(crate) fn select_language(_lang: &str) -> bool {
        false
    }
}

/* ------------------------------- macOS -------------------------------- */

#[cfg(target_os = "macos")]
mod macos {
    use super::primary_code;
    use core_foundation_sys::array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef};
    use core_foundation_sys::base::{Boolean, CFRelease, CFTypeRef, OSStatus};
    use core_foundation_sys::number::{CFBooleanGetValue, CFBooleanRef};
    use core_foundation_sys::string::{
        CFStringGetCString, CFStringGetLength, CFStringRef, kCFStringEncodingUTF8,
    };
    use std::os::raw::c_void;

    #[repr(C)]
    struct TISInputSource(c_void);
    type TISInputSourceRef = *mut TISInputSource;

    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn TISCopyCurrentKeyboardInputSource() -> TISInputSourceRef;
        fn TISCreateInputSourceList(
            properties: *const c_void,
            include_all_installed: Boolean,
        ) -> CFArrayRef;
        fn TISGetInputSourceProperty(
            input_source: TISInputSourceRef,
            property_key: CFStringRef,
        ) -> *mut c_void;
        fn TISSelectInputSource(input_source: TISInputSourceRef) -> OSStatus;

        static kTISPropertyInputSourceLanguages: CFStringRef;
        static kTISPropertyInputSourceType: CFStringRef;
        static kTISPropertyInputSourceIsSelectCapable: CFStringRef;
    }

    unsafe fn cfstring_to_string(s: CFStringRef) -> Option<String> {
        if s.is_null() {
            return None;
        }
        let len = unsafe { CFStringGetLength(s) };
        let cap = (len as usize) * 3 + 1;
        let mut buf = vec![0i8; cap];
        let ok =
            unsafe { CFStringGetCString(s, buf.as_mut_ptr(), cap as _, kCFStringEncodingUTF8) };
        if ok == 0 {
            return None;
        }
        let cstr = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) };
        cstr.to_str().ok().map(|s| s.to_owned())
    }

    /// first language of a source (the primary), normalized to ISO 639-1
    unsafe fn source_primary_lang(src: TISInputSourceRef) -> Option<String> {
        let arr = unsafe { TISGetInputSourceProperty(src, kTISPropertyInputSourceLanguages) }
            as CFArrayRef;
        if arr.is_null() || unsafe { CFArrayGetCount(arr) } == 0 {
            return None;
        }
        let first = unsafe { CFArrayGetValueAtIndex(arr, 0) } as CFStringRef;
        unsafe { cfstring_to_string(first) }.map(|t| primary_code(&t))
    }

    /// plain, selectable keyboard layouts only (excludes CJK IMEs)
    unsafe fn is_selectable_layout(src: TISInputSourceRef) -> bool {
        let ty =
            unsafe { TISGetInputSourceProperty(src, kTISPropertyInputSourceType) } as CFStringRef;
        let is_layout =
            unsafe { cfstring_to_string(ty) }.as_deref() == Some("TISTypeKeyboardLayout");
        let selc = unsafe { TISGetInputSourceProperty(src, kTISPropertyInputSourceIsSelectCapable) }
            as CFBooleanRef;
        let selectable = !selc.is_null() && unsafe { CFBooleanGetValue(selc) };
        is_layout && selectable
    }

    pub(crate) fn current_language() -> Option<String> {
        unsafe {
            let src = TISCopyCurrentKeyboardInputSource();
            if src.is_null() {
                return None;
            }
            let lang = source_primary_lang(src);
            CFRelease(src as CFTypeRef);
            lang
        }
    }

    pub(crate) fn select_language(lang: &str) -> bool {
        let want = primary_code(lang);
        unsafe {
            let list = TISCreateInputSourceList(std::ptr::null(), 0);
            if list.is_null() {
                return false;
            }
            let mut selected = false;
            let n = CFArrayGetCount(list);
            for i in 0..n {
                let src = CFArrayGetValueAtIndex(list, i) as TISInputSourceRef;
                if src.is_null() || !is_selectable_layout(src) {
                    continue;
                }
                if source_primary_lang(src).as_deref() == Some(want.as_str()) {
                    selected = TISSelectInputSource(src) == 0;
                    break;
                }
            }
            CFRelease(list as CFTypeRef);
            selected
        }
    }
}

/* ------------------------------ Windows ------------------------------- */

#[cfg(target_os = "windows")]
mod windows_impl {
    use super::primary_code;
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetKeyboardLayout, KLF_ACTIVATE, LoadKeyboardLayoutW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId, HWND_BROADCAST, PostMessageW,
        WM_INPUTLANGCHANGEREQUEST,
    };
    use windows::core::PCWSTR;

    /// PRIMARYLANGID -> ISO 639-1
    fn langid_to_iso(langid: u16) -> Option<&'static str> {
        Some(match langid & 0x3ff {
            0x09 => "en",
            0x01 => "ar",
            0x0c => "fr",
            0x07 => "de",
            0x0a => "es",
            0x19 => "ru",
            0x10 => "it",
            0x16 => "pt",
            0x1f => "tr",
            0x0d => "he",
            0x15 => "pl",
            0x13 => "nl",
            0x1d => "sv",
            0x08 => "el",
            0x04 => "zh",
            0x11 => "ja",
            0x12 => "ko",
            _ => return None,
        })
    }

    /// ISO 639-1 -> a default-sublang KLID string (e.g. "00000409")
    fn iso_to_klid(iso: &str) -> Option<&'static str> {
        Some(match iso {
            "en" => "00000409",
            "ar" => "00000401",
            "fr" => "0000040c",
            "de" => "00000407",
            "es" => "0000040a",
            "ru" => "00000419",
            "it" => "00000410",
            "pt" => "00000816",
            "tr" => "0000041f",
            "he" => "0000040d",
            "pl" => "00000415",
            "nl" => "00000413",
            "sv" => "0000041d",
            "el" => "00000408",
            "zh" => "00000804",
            "ja" => "00000411",
            "ko" => "00000412",
            _ => return None,
        })
    }

    pub(crate) fn current_language() -> Option<String> {
        unsafe {
            // GetForegroundWindow may be null; then tid==0 and
            // GetKeyboardLayout(0) returns the calling thread's layout.
            let hwnd = GetForegroundWindow();
            let tid = GetWindowThreadProcessId(hwnd, None);
            let hkl = GetKeyboardLayout(tid);
            let langid = (hkl.0 as usize & 0xffff) as u16;
            if langid == 0 {
                return None;
            }
            // low word carries the LANGID even for IME HKLs (high word set)
            langid_to_iso(langid).map(|s| s.to_string())
        }
    }

    pub(crate) fn select_language(lang: &str) -> bool {
        let Some(klid) = iso_to_klid(&primary_code(lang)) else {
            return false;
        };
        let wide: Vec<u16> = klid.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let hkl = match LoadKeyboardLayoutW(PCWSTR::from_raw(wide.as_ptr()), KLF_ACTIVATE) {
                Ok(h) => h,
                Err(_) => return false,
            };
            // KLF_ACTIVATE only switches the CALLING thread. The daemon
            // runs off the foreground thread, so ask the focused window
            // to switch its own input locale via a broadcast.
            let _ = PostMessageW(
                Some(HWND_BROADCAST),
                WM_INPUTLANGCHANGEREQUEST,
                WPARAM(0),
                LPARAM(hkl.0 as isize),
            );
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::primary_code;

    #[test]
    fn primary_code_normalizes() {
        assert_eq!(primary_code("en-US"), "en");
        assert_eq!(primary_code("zh-Hans"), "zh");
        assert_eq!(primary_code("AR"), "ar");
        assert_eq!(primary_code("fr"), "fr");
    }
}
