use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt::Display,
    task::{Poll, ready},
};

use async_trait::async_trait;
use futures::StreamExt;
use futures_core::Stream;

use input_event::{Event, KeyboardEvent, scancode};

pub use error::{CaptureCreationError, CaptureError, InputCaptureError};

pub mod error;

#[cfg(libei)]
mod libei;

#[cfg(target_os = "macos")]
mod macos;

/// cross-machine drag-and-drop: detect a file drag in progress locally
#[cfg(target_os = "macos")]
mod macos_drag;

/// Files currently being dragged on this machine (empty if none). Used
/// at the moment the cursor crosses to a peer so the drag can be carried
/// over. Only macOS can observe another app's drag today; elsewhere this
/// is always empty.
pub fn active_drag_files() -> Vec<std::path::PathBuf> {
    #[cfg(target_os = "macos")]
    {
        macos_drag::active_drag_files()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Vec::new()
    }
}

#[cfg(layer_shell)]
mod layer_shell;

#[cfg(windows)]
mod windows;

#[cfg(x11)]
mod x11;

/// fallback input capture (does not produce events)
mod dummy;

pub type CaptureHandle = u64;

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum CaptureEvent {
    /// capture on this capture handle is now active
    Begin,
    /// input event coming from capture handle
    Input(Event),
}

impl Display for CaptureEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureEvent::Begin => write!(f, "begin capture"),
            CaptureEvent::Input(e) => write!(f, "{e}"),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq)]
pub enum Position {
    Left,
    Right,
    Top,
    Bottom,
}

impl Position {
    pub fn opposite(&self) -> Self {
        match self {
            Position::Left => Self::Right,
            Position::Right => Self::Left,
            Position::Top => Self::Bottom,
            Position::Bottom => Self::Top,
        }
    }
}

impl Display for Position {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pos = match self {
            Position::Left => "left",
            Position::Right => "right",
            Position::Top => "top",
            Position::Bottom => "bottom",
        };
        write!(f, "{pos}")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backend {
    #[cfg(libei)]
    InputCapturePortal,
    #[cfg(layer_shell)]
    LayerShell,
    #[cfg(x11)]
    X11,
    #[cfg(windows)]
    Windows,
    #[cfg(target_os = "macos")]
    MacOs,
    Dummy,
}

impl Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(libei)]
            Backend::InputCapturePortal => write!(f, "input-capture-portal"),
            #[cfg(layer_shell)]
            Backend::LayerShell => write!(f, "layer-shell"),
            #[cfg(x11)]
            Backend::X11 => write!(f, "X11"),
            #[cfg(windows)]
            Backend::Windows => write!(f, "windows"),
            #[cfg(target_os = "macos")]
            Backend::MacOs => write!(f, "MacOS"),
            Backend::Dummy => write!(f, "dummy"),
        }
    }
}

pub struct InputCapture {
    /// capture backend
    capture: Box<dyn Capture>,
    /// keys pressed by active capture
    pressed_keys: HashSet<scancode::Linux>,
    /// map from position to ids
    position_map: HashMap<Position, Vec<CaptureHandle>>,
    /// map from id to position
    id_map: HashMap<CaptureHandle, Position>,
    /// pending events
    pending: VecDeque<(CaptureHandle, CaptureEvent)>,
}

impl InputCapture {
    /// create a new client with the given id
    pub async fn create(&mut self, id: CaptureHandle, pos: Position) -> Result<(), CaptureError> {
        if let Some(&old) = self.id_map.get(&id) {
            // a duplicate create used to be an assert, which (panic = abort)
            // took the whole app down; the existing capture stays as it is
            log::warn!("capture {id} already exists @ {old}, ignoring create @ {pos}");
            return Ok(());
        }

        self.id_map.insert(id, pos);

        if let Some(v) = self.position_map.get_mut(&pos) {
            v.push(id);
            Ok(())
        } else {
            self.position_map.insert(pos, vec![id]);
            self.capture.create(pos).await
        }
    }

    /// destroy the client with the given id, if it exists
    pub async fn destroy(&mut self, id: CaptureHandle) -> Result<(), CaptureError> {
        let Some(pos) = self.id_map.remove(&id) else {
            log::warn!("no capture {id} to destroy");
            return Ok(());
        };

        // Events already fanned out to every id at this position wait in
        // `pending`; one addressed to the destroyed id would reach a caller
        // that no longer knows it (v1.0.7 crashed on exactly that when an
        // incoming connection was re-registered at the active client's edge).
        self.pending.retain(|&(i, _)| i != id);

        log::debug!("destroying capture {id} @ {pos}");
        let remaining = self.position_map.get_mut(&pos).expect("id vector");
        remaining.retain(|&i| i != id);

        log::debug!("remaining ids @ {pos}: {remaining:?}");
        if remaining.is_empty() {
            log::debug!("destroying capture @ {pos} - no remaining ids");
            self.position_map.remove(&pos);
            self.capture.destroy(pos).await?;
        }
        Ok(())
    }

    /// release mouse
    pub async fn release(&mut self) -> Result<(), CaptureError> {
        self.pressed_keys.clear();
        self.capture.release().await
    }

    /// Drain and return every key the capture has forwarded as
    /// down-but-not-up. The caller is expected to synthesize key-up
    /// events to the remote peer for each — otherwise the peer
    /// retains phantom-held keys after capture is released. The
    /// canonical case is the release-bind chord
    /// (Ctrl+Shift+Alt+Meta): the down events were sent while
    /// capture was active, but the matching up events arrive after
    /// the local tap has flipped to passthrough and never reach
    /// the peer.
    pub fn take_pressed_keys(&mut self) -> HashSet<scancode::Linux> {
        std::mem::take(&mut self.pressed_keys)
    }

    /// destroy the input capture
    pub async fn terminate(&mut self) -> Result<(), CaptureError> {
        self.capture.terminate().await
    }

    /// creates a new [`InputCapture`]
    pub async fn new(backend: Option<Backend>) -> Result<Self, CaptureCreationError> {
        let capture = create(backend).await?;
        Ok(Self {
            capture,
            id_map: Default::default(),
            pending: Default::default(),
            position_map: Default::default(),
            pressed_keys: HashSet::new(),
        })
    }

    /// check whether the given keys are pressed
    pub fn keys_pressed(&self, keys: &[scancode::Linux]) -> bool {
        keys.iter().all(|k| self.pressed_keys.contains(k))
    }

    fn update_pressed_keys(&mut self, key: u32, state: u8) {
        if let Ok(scancode) = scancode::Linux::try_from(key) {
            log::debug!("key: {key}, state: {state}, scancode: {scancode:?}");
            match state {
                1 => self.pressed_keys.insert(scancode),
                _ => self.pressed_keys.remove(&scancode),
            };
        }
    }
}

impl Stream for InputCapture {
    type Item = Result<(CaptureHandle, CaptureEvent), CaptureError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        if let Some(e) = self.pending.pop_front() {
            return Poll::Ready(Some(Ok(e)));
        }

        loop {
            // ready
            let event = ready!(self.capture.poll_next_unpin(cx));

            // stream closed
            let event = match event {
                Some(e) => e,
                None => return Poll::Ready(None),
            };

            // error occurred
            let (pos, event) = match event {
                Ok(e) => e,
                Err(e) => return Poll::Ready(Some(Err(e))),
            };

            // handle key presses
            if let CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key { key, state, .. })) =
                event
            {
                self.update_pressed_keys(key, state);
            }

            let ids = match self.position_map.get(&pos) {
                Some(ids) if !ids.is_empty() => ids,
                // no capture left at this position (destroyed while the
                // event was queued): drop it and poll the next one. Returning
                // Pending here registered no waker and could stall the stream.
                _ => continue,
            };

            let first = ids[0];
            if ids.len() > 1 {
                let rest: Vec<CaptureHandle> = ids[1..].to_vec();
                for id in rest {
                    self.pending.push_back((id, event));
                }
            }
            return Poll::Ready(Some(Ok((first, event))));
        }
    }
}

#[async_trait]
trait Capture: Stream<Item = Result<(Position, CaptureEvent), CaptureError>> + Unpin {
    /// create a new client with the given id
    async fn create(&mut self, pos: Position) -> Result<(), CaptureError>;

    /// destroy the client with the given id, if it exists
    async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError>;

    /// release mouse
    async fn release(&mut self) -> Result<(), CaptureError>;

    /// destroy the input capture
    async fn terminate(&mut self) -> Result<(), CaptureError>;
}

async fn create_backend(
    backend: Backend,
) -> Result<
    Box<dyn Capture<Item = Result<(Position, CaptureEvent), CaptureError>>>,
    CaptureCreationError,
> {
    match backend {
        #[cfg(libei)]
        Backend::InputCapturePortal => Ok(Box::new(libei::LibeiInputCapture::new().await?)),
        #[cfg(layer_shell)]
        Backend::LayerShell => Ok(Box::new(layer_shell::LayerShellInputCapture::new()?)),
        #[cfg(x11)]
        Backend::X11 => Ok(Box::new(x11::X11InputCapture::new()?)),
        #[cfg(windows)]
        Backend::Windows => Ok(Box::new(windows::WindowsInputCapture::new())),
        #[cfg(target_os = "macos")]
        Backend::MacOs => Ok(Box::new(macos::MacOSInputCapture::new().await?)),
        Backend::Dummy => Ok(Box::new(dummy::DummyInputCapture::new())),
    }
}

async fn create(
    backend: Option<Backend>,
) -> Result<
    Box<dyn Capture<Item = Result<(Position, CaptureEvent), CaptureError>>>,
    CaptureCreationError,
> {
    if let Some(backend) = backend {
        let b = create_backend(backend).await;
        if b.is_ok() {
            log::info!("using capture backend: {backend}");
        }
        return b;
    }

    for backend in [
        #[cfg(libei)]
        Backend::InputCapturePortal,
        #[cfg(layer_shell)]
        Backend::LayerShell,
        #[cfg(x11)]
        Backend::X11,
        #[cfg(windows)]
        Backend::Windows,
        #[cfg(target_os = "macos")]
        Backend::MacOs,
    ] {
        match create_backend(backend).await {
            Ok(b) => {
                log::info!("using capture backend: {backend}");
                return Ok(b);
            }
            Err(e) if e.cancelled_by_user() => return Err(e),
            Err(e) => log::warn!("{backend} input capture backend unavailable: {e}"),
        }
    }
    Err(CaptureCreationError::NoAvailableBackend)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::task::Context;

    /// a backend that replays a fixed list of events
    struct Replay(VecDeque<(Position, CaptureEvent)>);

    impl Stream for Replay {
        type Item = Result<(Position, CaptureEvent), CaptureError>;
        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(self.0.pop_front().map(Ok))
        }
    }

    #[async_trait]
    impl Capture for Replay {
        async fn create(&mut self, _pos: Position) -> Result<(), CaptureError> {
            Ok(())
        }
        async fn destroy(&mut self, _pos: Position) -> Result<(), CaptureError> {
            Ok(())
        }
        async fn release(&mut self) -> Result<(), CaptureError> {
            Ok(())
        }
        async fn terminate(&mut self) -> Result<(), CaptureError> {
            Ok(())
        }
    }

    fn capture(events: Vec<(Position, CaptureEvent)>) -> InputCapture {
        InputCapture {
            capture: Box::new(Replay(events.into())),
            pressed_keys: HashSet::new(),
            position_map: Default::default(),
            id_map: Default::default(),
            pending: Default::default(),
        }
    }

    // the v1.0.7 crash: an event fanned out to two ids at one edge, one id
    // destroyed before its copy was taken
    #[tokio::test]
    async fn destroyed_id_gets_no_queued_event() {
        let mut c = capture(vec![
            (Position::Left, CaptureEvent::Begin),
            (Position::Left, CaptureEvent::Begin),
        ]);
        c.create(1, Position::Left).await.unwrap();
        c.create(2, Position::Left).await.unwrap();
        assert_eq!(c.next().await.unwrap().unwrap().0, 1);
        c.destroy(2).await.unwrap();
        assert_eq!(c.next().await.unwrap().unwrap().0, 1);
        assert!(c.next().await.is_none());
    }

    // an event for an edge with no capture left is skipped, not a stall
    #[tokio::test]
    async fn event_for_removed_position_is_skipped() {
        let mut c = capture(vec![
            (Position::Left, CaptureEvent::Begin),
            (Position::Right, CaptureEvent::Begin),
        ]);
        c.create(1, Position::Right).await.unwrap();
        assert_eq!(c.next().await.unwrap().unwrap().0, 1);
    }

    #[tokio::test]
    async fn duplicate_create_and_unknown_destroy_do_not_panic() {
        let mut c = capture(vec![]);
        c.create(1, Position::Left).await.unwrap();
        c.create(1, Position::Right).await.unwrap();
        c.destroy(7).await.unwrap();
        c.destroy(1).await.unwrap();
        c.destroy(1).await.unwrap();
        assert!(c.id_map.is_empty() && c.position_map.is_empty());
    }
}
