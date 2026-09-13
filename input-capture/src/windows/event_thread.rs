use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::ffi::c_void;
use std::ptr::addr_of_mut;

use std::default::Default;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::error::TrySendError;
use windows::Win32::Foundation::{FALSE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICEW, ENUM_CURRENT_SETTINGS,
    EnumDisplayDevicesW, EnumDisplaySettingsW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::{
    GetCurrentThread, GetCurrentThreadId, SetThreadInformation, SetThreadPriority,
    THREAD_POWER_THROTTLING_CURRENT_VERSION, THREAD_POWER_THROTTLING_EXECUTION_SPEED,
    THREAD_POWER_THROTTLING_STATE, THREAD_PRIORITY_TIME_CRITICAL, ThreadPowerThrottling,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::core::{PCWSTR, w};

use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DispatchMessageW, EDD_GET_DEVICE_INTERFACE_NAME, GetMessageW,
    HHOOK, HOOKPROC, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, MSG, MSLLHOOKSTRUCT, PostThreadMessageW,
    RegisterClassW, SetTimer, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
    WH_KEYBOARD_LL, WH_MOUSE_LL, WINDOW_STYLE, WM_DISPLAYCHANGE, WM_KEYDOWN, WM_KEYUP,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER, WM_USER,
    WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSW, WNDPROC,
};

use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, PointerEvent,
    scancode::{self, Linux},
};

use super::{CaptureEvent, Position, display_util};

pub(crate) struct EventThread {
    request_buffer: Arc<Mutex<Vec<ClientUpdate>>>,
    thread: Option<thread::JoinHandle<()>>,
    thread_id: u32,
}

impl EventThread {
    pub(crate) fn new(event_tx: Sender<(Position, CaptureEvent)>) -> Self {
        let request_buffer = Default::default();
        let (thread, thread_id) = start(event_tx, Arc::clone(&request_buffer));
        Self {
            request_buffer,
            thread: Some(thread),
            thread_id,
        }
    }

    pub(crate) fn release_capture(&self) {
        self.signal(RequestType::Release);
    }

    pub(crate) fn create(&self, pos: Position) {
        self.client_update(ClientUpdate::Create(pos));
    }

    pub(crate) fn destroy(&self, pos: Position) {
        self.client_update(ClientUpdate::Destroy(pos));
    }

    fn exit(&self) {
        self.signal(RequestType::Exit);
    }

    fn client_update(&self, request: ClientUpdate) {
        {
            let mut requests = self.request_buffer.lock().unwrap();
            requests.push(request);
        }
        self.signal(RequestType::ClientUpdate);
    }

    fn signal(&self, event_type: RequestType) {
        let id = self.thread_id;
        unsafe { PostThreadMessageW(id, WM_USER, WPARAM(event_type as usize), LPARAM(0)).unwrap() };
    }
}

impl Drop for EventThread {
    fn drop(&mut self) {
        self.exit();
        let _ = self.thread.take().expect("thread").join();
    }
}

enum RequestType {
    ClientUpdate = 0,
    Release = 1,
    Exit = 2,
}

enum ClientUpdate {
    Create(Position),
    Destroy(Position),
}

fn blocking_send_event(pos: Position, event: CaptureEvent) {
    EVENT_TX.with_borrow_mut(|tx| tx.as_mut().unwrap().blocking_send((pos, event)).unwrap())
}

fn try_send_event(
    pos: Position,
    event: CaptureEvent,
) -> Result<(), TrySendError<(Position, CaptureEvent)>> {
    EVENT_TX.with_borrow_mut(|tx| tx.as_mut().unwrap().try_send((pos, event)))
}

thread_local! {
    /// all configured clients
    static CLIENTS: RefCell<HashSet<Position>> = RefCell::new(HashSet::new());
    /// currently active client
    static ACTIVE_CLIENT: Cell<Option<Position>> = const { Cell::new(None) };
    /// input event channel
    static EVENT_TX: RefCell<Option<Sender<(Position, CaptureEvent)>>> = const { RefCell::new(None) };
    /// position of barrier entry
    static ENTRY_POINT: Cell<(i32, i32)> = const { Cell::new((0, 0)) };
    /// previous mouse position
    static PREV_POS: Cell<Option<(i32, i32)>> = const { Cell::new(None) };
    /// displays and generation counter
    static DISPLAYS: RefCell<(Vec<RECT>, i32)> = const { RefCell::new((Vec::new(), 0)) };
    /// the currently registered low level hooks (mouse, keyboard)
    static HOOKS: Cell<(Option<HHOOK>, Option<HHOOK>)> = const { Cell::new((None, None)) };
}

/// `GetTickCount()` at the last hook callback, used by the watchdog below.
static LAST_HOOK_TICK: AtomicU32 = AtomicU32::new(0);
/// `GetTickCount()` at the last (re)registration of the hooks
static LAST_INSTALL_TICK: AtomicU32 = AtomicU32::new(0);

/// watchdog timer, see [`check_hooks_alive`]
const WATCHDOG_TIMER_ID: usize = 1;
const WATCHDOG_INTERVAL_MS: u32 = 1000;
/// how far our hooks may lag the system's own last-input time before they
/// are considered dead
const HOOK_STALE_MS: i32 = 1500;
/// how often the hooks are refreshed while nothing is being captured
const HOOK_REFRESH_MS: i32 = 60_000;

/// Register both low level hooks, replacing any previous registration.
///
/// Called once at startup and again whenever the watchdog notices Windows
/// has dropped the hooks.
fn install_hooks() {
    unsafe {
        // release whatever is left of a previous registration
        let (mouse, keyboard) = HOOKS.with(|hooks| hooks.take());
        if let Some(hook) = mouse {
            let _ = UnhookWindowsHookEx(hook);
        }
        if let Some(hook) = keyboard {
            let _ = UnhookWindowsHookEx(hook);
        }

        let mouse_proc: HOOKPROC = Some(mouse_proc);
        let kybrd_proc: HOOKPROC = Some(kybrd_proc);
        let mouse = match SetWindowsHookExW(WH_MOUSE_LL, mouse_proc, None, 0) {
            Ok(hook) => Some(hook),
            Err(e) => {
                log::error!("failed to register mouse hook: {e}");
                None
            }
        };
        let keyboard = match SetWindowsHookExW(WH_KEYBOARD_LL, kybrd_proc, None, 0) {
            Ok(hook) => Some(hook),
            Err(e) => {
                log::error!("failed to register keyboard hook: {e}");
                None
            }
        };
        HOOKS.with(|hooks| hooks.set((mouse, keyboard)));
        // the hooks are live as of now - don't let the watchdog see the
        // previous (stale) timestamp and immediately reinstall again
        let now = GetTickCount();
        LAST_HOOK_TICK.store(now, Ordering::Relaxed);
        LAST_INSTALL_TICK.store(now, Ordering::Relaxed);
    }
}

fn uninstall_hooks() {
    let (mouse, keyboard) = HOOKS.with(|hooks| hooks.take());
    unsafe {
        if let Some(hook) = mouse {
            let _ = UnhookWindowsHookEx(hook);
        }
        if let Some(hook) = keyboard {
            let _ = UnhookWindowsHookEx(hook);
        }
    }
}

/// Detect hooks that Windows has silently removed and put them back.
///
/// A low level hook whose callback does not return within
/// `LowLevelHooksTimeout` (300 ms by default) is dropped by Windows without
/// any notification - the app just stops seeing input forever. That happens
/// exactly when the machine is busy or the process gets throttled, i.e.
/// while Vylo sits minimized in the tray. There is no API to ask whether a
/// hook is still installed, so compare our own last callback against the
/// system's last input time: if the system has seen input well after we
/// last did, the hooks are gone.
fn check_hooks_alive() {
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    if !unsafe { GetLastInputInfo(addr_of_mut!(info)) }.as_bool() {
        return;
    }
    let last_hook = LAST_HOOK_TICK.load(Ordering::Relaxed);
    // tick counts wrap every ~49 days, hence wrapping_sub
    let behind = info.dwTime.wrapping_sub(last_hook) as i32;
    if behind > HOOK_STALE_MS {
        log::warn!("input hooks missed {behind} ms of input - reinstalling");
        return install_hooks();
    }

    // Belt and braces: the check above cannot see Windows dropping only
    // one of the two hooks, because the surviving one keeps the timestamp
    // fresh. So re-register them from time to time anyway — but only while
    // no client is active, since the moment between unhook and hook could
    // otherwise leak a click to this machine instead of the peer.
    if ACTIVE_CLIENT.get().is_some() {
        return;
    }
    let since_install =
        unsafe { GetTickCount() }.wrapping_sub(LAST_INSTALL_TICK.load(Ordering::Relaxed)) as i32;
    if since_install > HOOK_REFRESH_MS {
        log::debug!("refreshing input hooks");
        install_hooks();
    }
}

/// Make this thread as hard to throttle as possible: the hook callbacks
/// have to answer within `LowLevelHooksTimeout` even when the process has no
/// window on screen and Windows would rather run it on an efficiency core.
fn prepare_hook_thread() {
    unsafe {
        let thread = GetCurrentThread();
        if let Err(e) = SetThreadPriority(thread, THREAD_PRIORITY_TIME_CRITICAL) {
            log::warn!("could not raise input thread priority: {e}");
        }
        let state = THREAD_POWER_THROTTLING_STATE {
            Version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: 0, // managed by us, throttling off
        };
        let res = SetThreadInformation(
            thread,
            ThreadPowerThrottling,
            &state as *const THREAD_POWER_THROTTLING_STATE as *const c_void,
            std::mem::size_of::<THREAD_POWER_THROTTLING_STATE>() as u32,
        );
        if let Err(e) = res {
            log::debug!("could not disable input thread throttling: {e}");
        }
    }
}

fn get_msg() -> Option<MSG> {
    unsafe {
        let mut msg = std::mem::zeroed();
        let ret = GetMessageW(addr_of_mut!(msg), None, 0, 0);
        match ret.0 {
            0 => None,
            x if x > 0 => Some(msg),
            _ => panic!("error in GetMessageW"),
        }
    }
}

fn start(
    event_tx: Sender<(Position, CaptureEvent)>,
    request_buffer: Arc<Mutex<Vec<ClientUpdate>>>,
) -> (thread::JoinHandle<()>, u32) {
    /* condition variable to wait for thead id */
    let thread_id = Arc::new((Condvar::new(), Mutex::new(None)));
    let thread_id_ = Arc::clone(&thread_id);

    let msg_thread = thread::spawn(|| start_routine(thread_id_, event_tx, request_buffer));

    /* wait for thread to set its id */
    let (cond, thread_id) = &*thread_id;
    let mut thread_id = thread_id.lock().unwrap();
    while (*thread_id).is_none() {
        thread_id = cond.wait(thread_id).expect("channel closed");
    }
    (msg_thread, thread_id.expect("thread id"))
}

fn start_routine(
    ready: Arc<(Condvar, Mutex<Option<u32>>)>,
    event_tx: Sender<(Position, CaptureEvent)>,
    request_buffer: Arc<Mutex<Vec<ClientUpdate>>>,
) {
    EVENT_TX.replace(Some(event_tx));
    /* communicate thread id */
    {
        let (cnd, mtx) = &*ready;
        let mut ready = mtx.lock().unwrap();
        *ready = Some(unsafe { GetCurrentThreadId() });
        cnd.notify_one();
    }

    let window_proc: WNDPROC = Some(window_proc);

    /* keep this thread responsive enough for the low level hooks */
    prepare_hook_thread();

    /* register hooks */
    install_hooks();

    let instance = unsafe { GetModuleHandleW(None).unwrap() };
    let instance = instance.into();
    let window_class: WNDCLASSW = WNDCLASSW {
        lpfnWndProc: window_proc,
        hInstance: instance,
        lpszClassName: w!("lan-mouse-message-window-class"),
        ..Default::default()
    };

    static WINDOW_CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);
    if WINDOW_CLASS_REGISTERED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        /* register window class if not yet done so */
        unsafe {
            let ret = RegisterClassW(&window_class);
            if ret == 0 {
                panic!("RegisterClassW");
            }
        }
    }

    /* window is used ro receive WM_DISPLAYCHANGE and WM_TIMER messages */
    let window = unsafe {
        CreateWindowExW(
            Default::default(),
            w!("lan-mouse-message-window-class"),
            w!("lan-mouse-msg-window"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )
        .expect("CreateWindowExW")
    };

    /* watchdog: put the hooks back if windows drops them */
    if unsafe { SetTimer(Some(window), WATCHDOG_TIMER_ID, WATCHDOG_INTERVAL_MS, None) } == 0 {
        log::warn!("failed to start the input hook watchdog");
    }

    /* run message loop */
    while let Some(msg) = get_msg() {
        // mouse / keybrd proc do not actually return a message
        if msg.hwnd.0.is_null() {
            /* messages sent via PostThreadMessage */
            match msg.wParam.0 {
                x if x == RequestType::Exit as usize => break,
                x if x == RequestType::Release as usize => {
                    ACTIVE_CLIENT.take();
                }
                x if x == RequestType::ClientUpdate as usize => {
                    let requests = {
                        let mut res = vec![];
                        let mut requests = request_buffer.lock().unwrap();
                        for request in requests.drain(..) {
                            res.push(request);
                        }
                        res
                    };

                    for request in requests {
                        update_clients(request)
                    }
                }
                _ => {}
            }
        } else {
            /* other messages for window_procs */
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    uninstall_hooks();
}

fn check_client_activation(wparam: WPARAM, lparam: LPARAM) -> bool {
    if wparam.0 != WM_MOUSEMOVE as usize {
        return ACTIVE_CLIENT.get().is_some();
    }
    let mouse_low_level: MSLLHOOKSTRUCT = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
    let curr_pos = (mouse_low_level.pt.x, mouse_low_level.pt.y);
    let prev_pos = PREV_POS.get().unwrap_or(curr_pos);
    PREV_POS.replace(Some(curr_pos));

    /* next event is the first actual event */
    let ret = ACTIVE_CLIENT.get().is_some();

    /* client already active, no need to check */
    if ACTIVE_CLIENT.get().is_some() {
        return ret;
    }

    /* check if a client was activated */
    let entered = DISPLAYS.with_borrow_mut(|(displays, generation)| {
        update_display_regions(displays, generation);
        display_util::entered_barrier(prev_pos, curr_pos, displays)
    });

    let Some(pos) = entered else {
        return ret;
    };

    /* check if a client is registered for the barrier */
    if !CLIENTS.with_borrow(|clients| clients.contains(&pos)) {
        return ret;
    }

    /* update active client and entry point */
    ACTIVE_CLIENT.replace(Some(pos));
    let entry_point = DISPLAYS.with_borrow(|(displays, _)| {
        display_util::clamp_to_display_bounds(displays, prev_pos, curr_pos)
    });
    ENTRY_POINT.replace(entry_point);

    /* notify main thread */
    log::debug!("ENTERED @ {prev_pos:?} -> {curr_pos:?}");
    let active = ACTIVE_CLIENT.get().expect("active client");
    blocking_send_event(active, CaptureEvent::Begin);

    ret
}

unsafe extern "system" fn mouse_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    /* first thing: tell the watchdog the hook is alive */
    LAST_HOOK_TICK.store(unsafe { GetTickCount() }, Ordering::Relaxed);

    let active = check_client_activation(wparam, lparam);

    /* no client was active */
    if !active {
        return CallNextHookEx(None, ncode, wparam, lparam);
    }

    /* get active client if any */
    let Some(pos) = ACTIVE_CLIENT.get() else {
        return LRESULT(1);
    };

    /* convert to lan-mouse event */
    let Some(pointer_event) = to_mouse_event(wparam, lparam) else {
        return LRESULT(1);
    };

    /* notify mainthread (drop events if sending too fast) */
    if let Err(e) = try_send_event(pos, CaptureEvent::Input(Event::Pointer(pointer_event))) {
        log::warn!("e: {e}");
    }

    /* don't pass event to applications */
    LRESULT(1)
}

unsafe extern "system" fn kybrd_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    /* first thing: tell the watchdog the hook is alive */
    LAST_HOOK_TICK.store(unsafe { GetTickCount() }, Ordering::Relaxed);

    /* get active client if any */
    let Some(client) = ACTIVE_CLIENT.get() else {
        return CallNextHookEx(None, ncode, wparam, lparam);
    };

    /* convert to key event */
    let Some(key_event) = to_key_event(wparam, lparam) else {
        return LRESULT(1);
    };

    if let Err(e) = try_send_event(client, CaptureEvent::Input(Event::Keyboard(key_event))) {
        log::warn!("e: {e}");
    }

    /* don't pass event to applications */
    LRESULT(1)
}

unsafe extern "system" fn window_proc(
    _hwnd: HWND,
    uint: u32,
    wparam: WPARAM,
    _lparam: LPARAM,
) -> LRESULT {
    if uint == WM_DISPLAYCHANGE {
        log::debug!("display resolution changed");
        DISPLAY_RESOLUTION_GENERATION.fetch_add(1, Ordering::Release);
    } else if uint == WM_TIMER && wparam.0 == WATCHDOG_TIMER_ID {
        check_hooks_alive();
    }
    LRESULT(1)
}

static DISPLAY_RESOLUTION_GENERATION: AtomicI32 = AtomicI32::new(1);

fn update_display_regions(displays: &mut Vec<RECT>, generation: &mut i32) {
    let global_generation = DISPLAY_RESOLUTION_GENERATION.load(Ordering::Acquire);
    if *generation != global_generation {
        enumerate_displays(displays);
        log::debug!("displays: {displays:?}");
        *generation = global_generation;
    }
}

fn enumerate_displays(display_rects: &mut Vec<RECT>) {
    display_rects.clear();
    unsafe {
        let mut devices = vec![];
        for i in 0.. {
            let mut device: DISPLAY_DEVICEW = std::mem::zeroed();
            device.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
            let ret = EnumDisplayDevicesW(None, i, &mut device, EDD_GET_DEVICE_INTERFACE_NAME);
            if ret == FALSE {
                break;
            }
            if device
                .StateFlags
                .contains(DISPLAY_DEVICE_ATTACHED_TO_DESKTOP)
            {
                devices.push(device.DeviceName);
            }
        }
        for device in devices {
            let mut dev_mode: DEVMODEW = std::mem::zeroed();
            dev_mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
            let ret = EnumDisplaySettingsW(
                PCWSTR::from_raw(&device as *const _),
                ENUM_CURRENT_SETTINGS,
                &mut dev_mode,
            );
            if ret == FALSE {
                log::warn!("no display mode");
            }

            let pos = dev_mode.Anonymous1.Anonymous2.dmPosition;
            let (x, y) = (pos.x, pos.y);
            let (width, height) = (dev_mode.dmPelsWidth, dev_mode.dmPelsHeight);

            display_rects.push(RECT {
                left: x,
                right: x + width as i32,
                top: y,
                bottom: y + height as i32,
            });
        }
    }
}

fn update_clients(request: ClientUpdate) {
    match request {
        ClientUpdate::Create(pos) => {
            CLIENTS.with_borrow_mut(|clients| clients.insert(pos));
        }
        ClientUpdate::Destroy(pos) => {
            if let Some(active_pos) = ACTIVE_CLIENT.get() {
                if pos == active_pos {
                    let _ = ACTIVE_CLIENT.take();
                }
            }
            CLIENTS.with_borrow_mut(|clients| clients.remove(&pos));
        }
    }
}

fn to_key_event(wparam: WPARAM, lparam: LPARAM) -> Option<KeyboardEvent> {
    let kybrdllhookstruct: KBDLLHOOKSTRUCT = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
    let mut scan_code = kybrdllhookstruct.scanCode;
    log::trace!("scan_code: {scan_code}");
    if kybrdllhookstruct.flags.contains(LLKHF_EXTENDED) {
        scan_code |= 0xE000;
    }
    let Ok(win_scan_code) = scancode::Windows::try_from(scan_code) else {
        log::warn!("failed to translate to windows scancode: {scan_code}");
        return None;
    };
    log::trace!("windows_scan: {win_scan_code:?}");
    let Ok(linux_scan_code): Result<Linux, ()> = win_scan_code.try_into() else {
        log::warn!("failed to translate into linux scancode: {win_scan_code:?}");
        return None;
    };
    log::trace!("windows_scan: {linux_scan_code:?}");
    let scan_code = linux_scan_code as u32;
    match wparam {
        WPARAM(p) if p == WM_KEYDOWN as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 1,
        }),
        WPARAM(p) if p == WM_KEYUP as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 0,
        }),
        WPARAM(p) if p == WM_SYSKEYDOWN as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 1,
        }),
        WPARAM(p) if p == WM_SYSKEYUP as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 0,
        }),
        _ => None,
    }
}

fn to_mouse_event(wparam: WPARAM, lparam: LPARAM) -> Option<PointerEvent> {
    let mouse_low_level: MSLLHOOKSTRUCT = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
    match wparam {
        WPARAM(p) if p == WM_LBUTTONDOWN as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_LEFT,
            state: 1,
        }),
        WPARAM(p) if p == WM_MBUTTONDOWN as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_MIDDLE,
            state: 1,
        }),
        WPARAM(p) if p == WM_RBUTTONDOWN as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_RIGHT,
            state: 1,
        }),
        WPARAM(p) if p == WM_LBUTTONUP as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_LEFT,
            state: 0,
        }),
        WPARAM(p) if p == WM_MBUTTONUP as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_MIDDLE,
            state: 0,
        }),
        WPARAM(p) if p == WM_RBUTTONUP as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_RIGHT,
            state: 0,
        }),
        WPARAM(p) if p == WM_MOUSEMOVE as usize => {
            let (x, y) = (mouse_low_level.pt.x, mouse_low_level.pt.y);
            let (ex, ey) = ENTRY_POINT.get();
            let (dx, dy) = (x - ex, y - ey);
            let (dx, dy) = (dx as f64, dy as f64);
            Some(PointerEvent::Motion { time: 0, dx, dy })
        }
        WPARAM(p) if p == WM_MOUSEWHEEL as usize => Some(PointerEvent::AxisDiscrete120 {
            axis: 0,
            value: -(mouse_low_level.mouseData as i32 >> 16),
        }),
        WPARAM(p) if p == WM_XBUTTONDOWN as usize || p == WM_XBUTTONUP as usize => {
            let hb = mouse_low_level.mouseData >> 16;
            let button = match hb {
                1 => BTN_BACK,
                2 => BTN_FORWARD,
                _ => {
                    log::warn!("unknown mouse button");
                    return None;
                }
            };
            Some(PointerEvent::Button {
                time: 0,
                button,
                state: if p == WM_XBUTTONDOWN as usize { 1 } else { 0 },
            })
        }
        WPARAM(p) if p == WM_MOUSEHWHEEL as usize => Some(PointerEvent::AxisDiscrete120 {
            axis: 1, // Horizontal
            value: mouse_low_level.mouseData as i32 >> 16,
        }),
        w => {
            log::warn!("unknown mouse event: {w:?}");
            None
        }
    }
}
