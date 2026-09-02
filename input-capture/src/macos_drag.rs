//! Detect a file drag in progress on this Mac so it can be carried across
//! to the peer (cross-machine drag-and-drop).
//!
//! macOS exposes the files of an in-flight drag on the global DRAG
//! pasteboard ("Apple CFPasteboard drag"), readable by any process in
//! the session from any thread (no main-thread requirement, no TCC
//! prompt). Two gotchas shape this module:
//!
//! 1. The drag pasteboard is NEVER cleared after a drop or cancel — it
//!    keeps the last drag's items indefinitely, so "has items" says
//!    nothing. What does change is its generation, which the source app
//!    bumps at drag START. We therefore treat "the generation changed
//!    since the last left-mouse-up" as "a drag started during this
//!    button hold", and consume the generation on every mouse-up.
//! 2. Finder puts file-REFERENCE URLs (`file:///.file/id=N.M`) on the
//!    pasteboard; they must be resolved with
//!    `CFURLGetFileSystemRepresentation`, not by stripping `file://`.
//!
//! Once a drag is recognised it stays "armed" until the button is
//! released, so a cursor that returns to the Mac and crosses again mid-
//! drag is still recognised.
#![allow(non_upper_case_globals)]

use core_foundation_sys::array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef};
use core_foundation_sys::base::{
    Boolean, CFComparisonResult, CFIndex, CFRelease, OSStatus, kCFAllocatorDefault,
};
use core_foundation_sys::data::{CFDataGetBytePtr, CFDataGetLength, CFDataRef};
use core_foundation_sys::string::{
    CFStringCompare, CFStringCreateWithCString, CFStringRef, kCFStringEncodingUTF8,
};
use core_foundation_sys::url::{CFURLCreateWithBytes, CFURLGetFileSystemRepresentation, CFURLRef};
use std::ffi::{CStr, OsStr, c_void};
use std::os::raw::c_char;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Mutex;

#[repr(C)]
struct OpaquePasteboard(c_void);
type PasteboardRef = *mut OpaquePasteboard;
type PasteboardItemID = *mut c_void;
type ItemCount = std::os::raw::c_ulong;
type PasteboardSyncFlags = u32;
type PasteboardFlavorFlags = u32;

const kPasteboardModified: PasteboardSyncFlags = 1 << 0;
const kPasteboardFlavorPromised: PasteboardFlavorFlags = 1 << 9;
const badPasteboardSyncErr: OSStatus = -25130;

/// runtime value of NSPasteboardNameDrag — NOT kPasteboardUniqueName
const DRAG_PASTEBOARD_NAME: &str = "Apple CFPasteboard drag";
const UTI_FILE_URL: &str = "public.file-url";

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn PasteboardCreate(name: CFStringRef, out: *mut PasteboardRef) -> OSStatus;
    fn PasteboardSynchronize(pb: PasteboardRef) -> PasteboardSyncFlags;
    fn PasteboardGetItemCount(pb: PasteboardRef, out: *mut ItemCount) -> OSStatus;
    fn PasteboardGetItemIdentifier(
        pb: PasteboardRef,
        index: CFIndex, // 1-based
        out: *mut PasteboardItemID,
    ) -> OSStatus;
    fn PasteboardCopyItemFlavors(
        pb: PasteboardRef,
        item: PasteboardItemID,
        out: *mut CFArrayRef,
    ) -> OSStatus;
    fn PasteboardGetItemFlavorFlags(
        pb: PasteboardRef,
        item: PasteboardItemID,
        flavor: CFStringRef,
        out: *mut PasteboardFlavorFlags,
    ) -> OSStatus;
    fn PasteboardCopyItemFlavorData(
        pb: PasteboardRef,
        item: PasteboardItemID,
        flavor: CFStringRef,
        out: *mut CFDataRef,
    ) -> OSStatus;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceButtonState(state_id: u32, button: u32) -> bool;
}

/// left mouse button held anywhere in the session (all processes)
fn left_button_down() -> bool {
    // kCGEventSourceStateCombinedSessionState = 0, kCGMouseButtonLeft = 0
    unsafe { CGEventSourceButtonState(0, 0) }
}

unsafe fn cfstr(s: &str) -> CFStringRef {
    let c = std::ffi::CString::new(s).expect("no interior nul");
    unsafe {
        CFStringCreateWithCString(
            kCFAllocatorDefault,
            c.as_ptr() as *const c_char,
            kCFStringEncodingUTF8,
        )
    }
}

/// one long-lived pasteboard handle; the modification flag returned by
/// `PasteboardSynchronize` is relative to this handle's previous sync
struct DragPasteboard {
    pb: PasteboardRef,
    file_url_uti: CFStringRef,
}
// moved into the global Mutex; never used concurrently
unsafe impl Send for DragPasteboard {}

impl DragPasteboard {
    fn new() -> Option<Self> {
        unsafe {
            let name = cfstr(DRAG_PASTEBOARD_NAME);
            let mut pb: PasteboardRef = std::ptr::null_mut();
            let st = PasteboardCreate(name, &mut pb);
            CFRelease(name as *const c_void);
            if st != 0 || pb.is_null() {
                log::warn!("drag pasteboard unavailable (OSStatus {st}); drag-and-drop disabled");
                return None;
            }
            PasteboardSynchronize(pb); // prime the baseline
            Some(Self {
                pb,
                file_url_uti: cfstr(UTI_FILE_URL),
            })
        }
    }

    /// consume the current generation (baseline for the next check)
    fn sync(&self) -> bool {
        unsafe { PasteboardSynchronize(self.pb) & kPasteboardModified != 0 }
    }

    /// (modified since last sync, resolved file paths)
    fn read(&self) -> (bool, Vec<PathBuf>) {
        unsafe {
            let modified = self.sync();
            let mut count: ItemCount = 0;
            if PasteboardGetItemCount(self.pb, &mut count) != 0 {
                return (modified, Vec::new());
            }
            let mut files = Vec::new();
            for i in 1..=(count as CFIndex) {
                let mut item: PasteboardItemID = std::ptr::null_mut();
                if PasteboardGetItemIdentifier(self.pb, i, &mut item) != 0 {
                    continue;
                }
                let mut flavors: CFArrayRef = std::ptr::null();
                if PasteboardCopyItemFlavors(self.pb, item, &mut flavors) != 0 || flavors.is_null()
                {
                    continue;
                }
                let has_file_url = (0..CFArrayGetCount(flavors)).any(|j| {
                    let f = CFArrayGetValueAtIndex(flavors, j) as CFStringRef;
                    matches!(
                        CFStringCompare(f, self.file_url_uti, 0),
                        CFComparisonResult::EqualTo
                    )
                });
                CFRelease(flavors as *const c_void);
                if !has_file_url {
                    continue;
                }
                // promised flavors have no data until dropped (Photos, Mail…)
                let mut flags: PasteboardFlavorFlags = 0;
                PasteboardGetItemFlavorFlags(self.pb, item, self.file_url_uti, &mut flags);
                if flags & kPasteboardFlavorPromised != 0 {
                    continue;
                }
                let mut data: CFDataRef = std::ptr::null();
                let st = PasteboardCopyItemFlavorData(self.pb, item, self.file_url_uti, &mut data);
                if st == badPasteboardSyncErr {
                    // pasteboard changed under us: resync and retry once
                    return self.read();
                }
                if st != 0 || data.is_null() {
                    continue;
                }
                if let Some(p) = file_url_data_to_path(data) {
                    files.push(p);
                }
                CFRelease(data as *const c_void);
            }
            (modified, files)
        }
    }
}

impl Drop for DragPasteboard {
    fn drop(&mut self) {
        unsafe {
            CFRelease(self.pb as *const c_void);
            CFRelease(self.file_url_uti as *const c_void);
        }
    }
}

/// CFData holds the UTF-8 URL string; CFURLGetFileSystemRepresentation
/// both percent-decodes and resolves Finder's `/.file/id=` reference URLs
unsafe fn file_url_data_to_path(data: CFDataRef) -> Option<PathBuf> {
    unsafe {
        let url: CFURLRef = CFURLCreateWithBytes(
            kCFAllocatorDefault,
            CFDataGetBytePtr(data),
            CFDataGetLength(data),
            kCFStringEncodingUTF8,
            std::ptr::null(),
        );
        if url.is_null() {
            return None;
        }
        let mut buf = vec![0u8; 4096];
        let ok: Boolean =
            CFURLGetFileSystemRepresentation(url, 1, buf.as_mut_ptr(), buf.len() as CFIndex);
        CFRelease(url as *const c_void);
        if ok == 0 {
            return None;
        }
        let c = CStr::from_bytes_until_nul(&buf).ok()?;
        Some(PathBuf::from(OsStr::from_bytes(c.to_bytes())))
    }
}

struct DragState {
    pb: DragPasteboard,
    /// a drag was recognised during the current button hold
    armed: bool,
    files: Vec<PathBuf>,
}

static STATE: Mutex<Option<Option<DragState>>> = Mutex::new(None);

fn with_state<R>(f: impl FnOnce(&mut DragState) -> R) -> Option<R> {
    let mut guard = STATE.lock().ok()?;
    if guard.is_none() {
        *guard = Some(DragPasteboard::new().map(|pb| DragState {
            pb,
            armed: false,
            files: Vec::new(),
        }));
    }
    guard.as_mut()?.as_mut().map(f)
}

/// Call on every left-mouse-up seen by the event tap (captured or not):
/// the drag — cross-machine or purely local — is over, so consume the
/// pasteboard generation and disarm.
pub(crate) fn on_left_mouse_up() {
    with_state(|s| {
        s.pb.sync();
        s.armed = false;
        s.files.clear();
    });
}

/// Files being dragged right now, or empty if no file drag is in
/// progress. Cheap; called at the moment the cursor crosses to the peer.
pub fn active_drag_files() -> Vec<PathBuf> {
    with_state(|s| {
        if !left_button_down() {
            s.armed = false;
            s.files.clear();
            return Vec::new();
        }
        let (modified, files) = s.pb.read();
        if modified && !files.is_empty() {
            s.armed = true;
            s.files = files;
        }
        if s.armed { s.files.clone() } else { Vec::new() }
    })
    .unwrap_or_default()
}
