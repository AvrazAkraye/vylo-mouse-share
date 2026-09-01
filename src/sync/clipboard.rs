//! Clipboard monitoring and application.
//!
//! Exactly one thread owns the OS clipboard handle. It polls for local
//! changes (text every tick, images every other tick since reading an
//! image is a full copy) and applies remote content pushed to it by the
//! sync actor.
//!
//! Echo prevention: fingerprints of the last content applied from the
//! peer (both as sent, and as read back after the OS round-trip, which
//! may re-encode images) and of the last content sent are remembered;
//! matching content is never re-broadcast, so no copy loop can form
//! between the machines.

use arboard::{Clipboard, ImageData};
use sha2::{Digest, Sha256};
use std::{
    borrow::Cow,
    io::Cursor,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, RecvTimeoutError, Sender, channel},
    },
    time::Duration,
};
use tokio::sync::mpsc::UnboundedSender;

const POLL_INTERVAL: Duration = Duration::from_millis(400);

/// remote content to put on the local clipboard
pub(crate) enum Apply {
    Text(String),
    /// png-encoded image (dimensions come from the png itself)
    Image {
        png: Vec<u8>,
    },
}

/// local clipboard change to forward to the peer
pub(crate) enum Change {
    Text(String),
    Image {
        width: u32,
        height: u32,
        png: Vec<u8>,
    },
}

pub(crate) struct ClipboardMonitor {
    apply_tx: Sender<Apply>,
}

impl ClipboardMonitor {
    pub(crate) fn new(change_tx: UnboundedSender<Change>, enabled: Arc<AtomicBool>) -> Self {
        let (apply_tx, apply_rx) = channel();
        std::thread::Builder::new()
            .name("vylo-clipboard".into())
            .spawn(move || monitor(apply_rx, change_tx, enabled))
            .expect("failed to spawn clipboard thread");
        Self { apply_tx }
    }

    pub(crate) fn apply(&self, apply: Apply) {
        let _ = self.apply_tx.send(apply);
    }
}

fn fingerprint_text(text: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"text");
    h.update(text.as_bytes());
    h.finalize().into()
}

/// cheap image fingerprint: dimensions, length and a prefix of the
/// pixel data — enough to detect change without hashing 30 MB every
/// poll
fn fingerprint_image(width: usize, height: usize, rgba: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"image");
    h.update((width as u64).to_le_bytes());
    h.update((height as u64).to_le_bytes());
    h.update((rgba.len() as u64).to_le_bytes());
    h.update(&rgba[..rgba.len().min(64 * 1024)]);
    h.finalize().into()
}

/// fingerprints that must not be (re-)broadcast
#[derive(Default)]
struct Suppressed {
    applied: Option<[u8; 32]>,
    applied_roundtrip: Option<[u8; 32]>,
    sent: Option<[u8; 32]>,
}

impl Suppressed {
    fn contains(&self, fp: &[u8; 32]) -> bool {
        self.applied.as_ref() == Some(fp)
            || self.applied_roundtrip.as_ref() == Some(fp)
            || self.sent.as_ref() == Some(fp)
    }
}

fn encode_png(width: usize, height: usize, rgba: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(Cursor::new(&mut out), width as u32, height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().ok()?;
    writer.write_image_data(rgba).ok()?;
    writer.finish().ok()?;
    Some(out)
}

fn decode_png(png_bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let decoder = png::Decoder::new(Cursor::new(png_bytes));
    let mut reader = decoder.read_info().ok()?;
    let info = reader.info();
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        log::warn!("peer sent a non-rgba8 clipboard image, ignoring");
        return None;
    }
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buf).ok()?;
    buf.truncate(frame.buffer_size());
    Some((frame.width, frame.height, buf))
}

fn monitor(
    apply_rx: Receiver<Apply>,
    change_tx: UnboundedSender<Change>,
    enabled: Arc<AtomicBool>,
) {
    let mut clipboard = match Clipboard::new() {
        Ok(c) => c,
        Err(e) => {
            log::error!("clipboard unavailable, clipboard sync disabled: {e}");
            return;
        }
    };
    let mut suppressed = Suppressed::default();
    // fingerprint of the current clipboard content as of the last poll
    let mut last_seen: Option<[u8; 32]> = None;
    let mut check_image = false;

    loop {
        /* apply remote content, blocking up to one poll interval */
        match apply_rx.recv_timeout(POLL_INTERVAL) {
            Ok(apply) => {
                apply_remote(&mut clipboard, apply, &mut suppressed, &mut last_seen);
                // drain any further queued items before polling
                while let Ok(apply) = apply_rx.try_recv() {
                    apply_remote(&mut clipboard, apply, &mut suppressed, &mut last_seen);
                }
                continue;
            }
            Err(RecvTimeoutError::Timeout) => (),
            Err(RecvTimeoutError::Disconnected) => return,
        }

        if !enabled.load(Ordering::SeqCst) {
            continue;
        }

        /* poll for local changes */
        if let Ok(text) = clipboard.get_text() {
            if !text.is_empty() {
                let fp = fingerprint_text(&text);
                if last_seen != Some(fp) {
                    last_seen = Some(fp);
                    if !suppressed.contains(&fp) {
                        suppressed.sent = Some(fp);
                        let _ = change_tx.send(Change::Text(text));
                    }
                }
                continue;
            }
        }

        check_image = !check_image;
        if !check_image {
            continue;
        }
        if let Ok(image) = clipboard.get_image() {
            let fp = fingerprint_image(image.width, image.height, &image.bytes);
            if last_seen != Some(fp) {
                last_seen = Some(fp);
                if !suppressed.contains(&fp) {
                    match encode_png(image.width, image.height, &image.bytes) {
                        Some(png) => {
                            suppressed.sent = Some(fp);
                            let _ = change_tx.send(Change::Image {
                                width: image.width as u32,
                                height: image.height as u32,
                                png,
                            });
                        }
                        None => log::warn!("failed to encode clipboard image"),
                    }
                }
            }
        }
    }
}

fn apply_remote(
    clipboard: &mut Clipboard,
    apply: Apply,
    suppressed: &mut Suppressed,
    last_seen: &mut Option<[u8; 32]>,
) {
    match apply {
        Apply::Text(text) => {
            let fp = fingerprint_text(&text);
            suppressed.applied = Some(fp);
            suppressed.applied_roundtrip = None;
            if let Err(e) = clipboard.set_text(&text) {
                log::warn!("failed to set clipboard text: {e}");
                return;
            }
            // what we read back is what future polls will see
            if let Ok(text) = clipboard.get_text() {
                let fp = fingerprint_text(&text);
                suppressed.applied_roundtrip = Some(fp);
                *last_seen = Some(fp);
            }
        }
        Apply::Image { png } => {
            let Some((width, height, rgba)) = decode_png(&png) else {
                return;
            };
            let fp = fingerprint_image(width as usize, height as usize, &rgba);
            suppressed.applied = Some(fp);
            suppressed.applied_roundtrip = None;
            let image = ImageData {
                width: width as usize,
                height: height as usize,
                bytes: Cow::Owned(rgba),
            };
            if let Err(e) = clipboard.set_image(image) {
                log::warn!("failed to set clipboard image: {e}");
                return;
            }
            // the OS may re-encode the image; remember the fingerprint
            // of what actually landed on the clipboard so the next poll
            // does not bounce it back to the peer
            if let Ok(image) = clipboard.get_image() {
                let fp = fingerprint_image(image.width, image.height, &image.bytes);
                suppressed.applied_roundtrip = Some(fp);
                *last_seen = Some(fp);
            }
        }
    }
}
