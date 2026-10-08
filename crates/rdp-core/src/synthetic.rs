//! Synthetic desktop backend — Phase 0 protocol spike.
//!
//! Renders a deterministic test pattern (moving bar + resolution banner grid)
//! with no Wayland involvement at all, so protocol bugs can never hide behind
//! compositor behavior. Honors `resize` trivially: the "virtual output" is
//! just a reallocation.

use std::sync::{Arc, Mutex};

use desk_backend::{
    BackendError, CaptureSession, CaptureTarget, DesktopBackend, Frame, FrameMeta, FrameTx,
    InputEvent, PixelFormat, Result, SessionRequest, Size,
};

pub struct SyntheticBackend;

impl DesktopBackend for SyntheticBackend {
    fn name(&self) -> &'static str {
        "synthetic"
    }

    fn supports_dynamic_resize(&self) -> bool {
        true
    }

    fn open(&self, req: SessionRequest) -> Result<Box<dyn CaptureSession>> {
        let size = match req.target {
            CaptureTarget::NewVirtualOutput(size) => size,
            CaptureTarget::ExistingOutput { .. } => req.initial_size,
        };
        let size = if size.width == 0 || size.height == 0 {
            req.initial_size
        } else {
            size
        };
        Ok(Box::new(SyntheticSession {
            size: Arc::new(Mutex::new(size)),
        }))
    }
}

struct SyntheticSession {
    /// Shared with the render thread so `resize` applies to new frames
    /// instead of only to the snapshot taken in `start` (F4).
    size: Arc<Mutex<Size>>,
}

impl CaptureSession for SyntheticSession {
    fn start(&mut self, tx: FrameTx) -> Result<()> {
        // 30 fps ticker; each send hands the peer thread a frame. The
        // moving-bar frame counter now increments so D3-visible motion works.
        let size = Arc::clone(&self.size);
        std::thread::spawn(move || {
            let mut frame_no: u64 = 0;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(33));
                frame_no = frame_no.wrapping_add(1);
                let size = *size.lock().unwrap_or_else(|e| e.into_inner());
                let frame = render_frame(size, frame_no);
                if tx.send(frame).is_err() {
                    break; // peer gone
                }
            }
        });
        Ok(())
    }

    fn resize(&mut self, size: Size) -> Result<()> {
        if size.width == 0 || size.height == 0 {
            return Err(BackendError::Unavailable("zero-size resize".into()));
        }
        *self.size.lock().unwrap_or_else(|e| e.into_inner()) = size;
        Ok(())
    }

    fn inject(&mut self, input: desk_backend::StampedInput) -> Result<()> {
        // D4: input decoding is wired end-to-end; the synthetic desktop
        // can't react, but logging proves the plumbing (rate-limited so the
        // pointer-motion flood doesn't swamp the real events).
        static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let log = match &input.event {
            InputEvent::PointerMotion { .. } => n % 60 == 0,
            _ => true,
        };
        if log {
            eprintln!("[synthetic] inject #{n}: {:?}", input.event);
        }
        Ok(())
    }
}

/// Deterministic pattern: blue gradient background, white grid every 64px,
/// moving vertical bar so frame pumping is visible, size-coded corner box.
fn render_frame(size: Size, frame_no: u64) -> Frame {
    let (w, h) = (size.width as usize, size.height as usize);
    let stride = w * 4;
    let mut px = vec![0u8; stride * h];
    let bar_x = ((frame_no / 2) % w.max(1) as u64) as usize;
    for y in 0..h {
        for x in 0..w {
            let i = y * stride + x * 4;
            // BGRA: gradient blue background
            px[i] = (32 + (x * 96 / w.max(1))) as u8; // B
            px[i + 1] = (16 + (y * 48 / h.max(1))) as u8; // G
            px[i + 2] = 8; // R
            px[i + 3] = 0xFF;
            if x % 64 == 0 || y % 64 == 0 {
                px[i] = 200;
                px[i + 1] = 200;
                px[i + 2] = 200;
            }
            if x.abs_diff(bar_x) < 4 {
                px[i] = 60;
                px[i + 1] = 220;
                px[i + 2] = 60;
            }
        }
    }
    Frame {
        meta: FrameMeta {
            size,
            stride,
            format: PixelFormat::Bgra32,
            damage: vec![],      // full surface; damage arrives with EGFX
            capture_stamp_ms: 0, // synthetic frames are unstamped
        },
        pixels: std::sync::Arc::new(px.into_boxed_slice()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_requested_size() {
        let f = render_frame(
            Size {
                width: 320,
                height: 200,
            },
            0,
        );
        assert_eq!(f.pixels.len(), 320 * 200 * 4);
        assert_eq!(f.meta.size.width, 320);
    }

    #[test]
    fn bar_moves() {
        let size = Size {
            width: 640,
            height: 64,
        };
        let a = render_frame(size, 0);
        let b = render_frame(size, 200);
        // Frame 200 puts the bar at x=100; x=100 is off the 64px grid, so
        // only the bar can differ there.
        assert_ne!(a.pixels[100 * 4], b.pixels[100 * 4]);
    }

    #[test]
    fn resize_rejects_zero() {
        let backend = SyntheticBackend;
        let size = Size {
            width: 1280,
            height: 800,
        };
        let mut s = backend.open(SessionRequest::without_client(size)).unwrap();
        assert!(s
            .resize(Size {
                width: 0,
                height: 600
            })
            .is_err());
        s.resize(Size {
            width: 1024,
            height: 768,
        })
        .unwrap();
    }

    #[test]
    fn resize_after_start_changes_rendered_frames() {
        // F4 regression: start() used to snapshot the size, so a later
        // resize() never reached the render loop.
        let backend = SyntheticBackend;
        let size = Size {
            width: 1280,
            height: 800,
        };
        let mut s = backend.open(SessionRequest::without_client(size)).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        s.start(tx).unwrap();
        let first = rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("first frame");
        assert_eq!(first.meta.size.width, 1280);
        s.resize(Size {
            width: 640,
            height: 480,
        })
        .unwrap();
        // Drain until the render thread picks the new size up (bounded).
        let mut saw_new = false;
        for _ in 0..100 {
            let f = rx
                .recv_timeout(std::time::Duration::from_millis(200))
                .expect("frame");
            if f.meta.size.width == 640 && f.pixels.len() == 640 * 480 * 4 {
                saw_new = true;
                break;
            }
        }
        assert!(saw_new, "resize after start must reach rendered frames");
        drop(rx); // stop the ticker
    }
}
