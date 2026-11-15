//! PipeWire capture (the "kde-pipewire" thread): consumes KWin's
//! zkde-screencast node on the user's session PipeWire daemon and turns its
//! buffers into desk-backend frames.
//!
//! Hyper-V rules, from field findings:
//! - **Never offer a DRM modifier.** On `hyperv_drm` with Mesa software
//!   rendering, a negotiated DmaBuf whose EGL import fails loops KWin's
//!   format negotiation forever. KWin only enters its DmaBuf path when the
//!   consumer's format carries `SPA_FORMAT_VIDEO_modifier`, so offering none
//!   keeps the stream on MemFd.
//! - Connect on the **shared** daemon (`connect(None)`), never a portal fd:
//!   the zkde node is published there, and portal private connections have
//!   a buffer-allocation bug on Hyper-V.
//! - Use `chunk.stride`, never `width * 4`: padded strides exist.
//! - KWin sends frames only on damage and also sends empty (cursor-only)
//!   buffers; both are normal.
//!
//! Commands arrive on a `pipewire::channel` attached to the loop, so they run
//! on this thread. Health summaries go back to the session task on a tokio
//! channel; pixels never leave this thread except as the delivered frame.

use std::cell::RefCell;
use std::io::Cursor;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use desk_backend::{Frame, FrameMeta, FrameTx, FrameWake, PixelFormat, Rect, Size};
use pipewire as pw;
use pw::spa;
use pw::spa::pod::{
    serialize::PodSerializer, ChoiceValue, Object, Pod, Property, PropertyFlags, Value,
};
use pw::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Id};

use crate::health::{sample_uniform, Sample, CONNECT_WINDOW};

pub enum Command {
    /// (Re)connect to this zkde node, dropping any current stream.
    Connect(u32),
    Disconnect,
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// A frame was delivered; `sample` is the health sample (only computed
    /// often inside the connect window, ~1/s after).
    Frame { sample: Sample, at: Instant },
    /// The stream died while we expected it to stream.
    StreamLost(Instant),
}

pub type CommandTx = pw::channel::Sender<Command>;
pub type EventTx = tokio::sync::mpsc::UnboundedSender<Event>;

/// Clip compositor damage rectangles to the frame. No damage metadata (or
/// none valid) means "everything changed".
pub fn clip_damage(regions: &[(i32, i32, u32, u32)], width: u32, height: u32) -> Vec<Rect> {
    let mut out = Vec::new();
    for &(x, y, w, h) in regions {
        if w == 0 || h == 0 {
            break; // spa terminator
        }
        let x0 = x.clamp(0, width as i32);
        let y0 = y.clamp(0, height as i32);
        let x1 = (x as i64 + w as i64).clamp(0, width as i64) as i32;
        let y1 = (y as i64 + h as i64).clamp(0, height as i64) as i32;
        if x1 > x0 && y1 > y0 {
            out.push(Rect {
                x: x0,
                y: y0,
                width: (x1 - x0) as u32,
                height: (y1 - y0) as u32,
            });
        }
    }
    if out.is_empty() {
        out.push(Rect {
            x: 0,
            y: 0,
            width,
            height,
        });
    }
    out
}

fn pod_bytes(obj: Object) -> Vec<u8> {
    PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(obj))
        .expect("serializing a fixed pod")
        .0
        .into_inner()
}

fn prop(key: u32, value: Value) -> Property {
    Property {
        key,
        flags: PropertyFlags::empty(),
        value,
    }
}

/// EnumFormat: BGRx/BGRA, any size, 0–60 fps. Deliberately no modifier
/// property (see the module docs).
fn enum_format() -> Vec<u8> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use spa::param::video::VideoFormat;
    pod_bytes(spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA
        ),
        spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle {
                width: 1920,
                height: 1080
            },
            spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            spa::utils::Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 60, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 60, denom: 1 }
        ),
    ))
}

/// Buffers (memfd / memptr only, 4–8 of them) plus header and damage metas.
fn buffer_params() -> [Vec<u8>; 3] {
    let sys = spa::sys::SPA_DATA_MemFd;
    let ptr = spa::sys::SPA_DATA_MemPtr;
    let buffers = Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamBuffers,
        id: spa::sys::SPA_PARAM_Buffers,
        properties: vec![
            prop(
                spa::sys::SPA_PARAM_BUFFERS_buffers,
                Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: 6,
                        min: 4,
                        max: 8,
                    },
                ))),
            ),
            prop(
                spa::sys::SPA_PARAM_BUFFERS_dataType,
                Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Flags {
                        default: 1 << sys,
                        flags: vec![1 << sys, 1 << ptr],
                    },
                ))),
            ),
        ],
    };
    let meta = |kind: u32, size: usize| Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamMeta,
        id: spa::sys::SPA_PARAM_Meta,
        properties: vec![
            prop(spa::sys::SPA_PARAM_META_type, Value::Id(Id(kind))),
            prop(spa::sys::SPA_PARAM_META_size, Value::Int(size as i32)),
        ],
    };
    [
        pod_bytes(buffers),
        pod_bytes(meta(
            spa::sys::SPA_META_Header,
            std::mem::size_of::<spa::sys::spa_meta_header>(),
        )),
        pod_bytes(meta(
            spa::sys::SPA_META_VideoDamage,
            16 * std::mem::size_of::<spa::sys::spa_meta_region>(),
        )),
    ]
}

struct StreamData {
    format: spa::param::video::VideoInfoRaw,
    frames: FrameTx,
    wake: Option<FrameWake>,
    events: EventTx,
    connected_at: Instant,
    last_sample: Option<Instant>,
    streaming: bool,
    debug: bool,
    stats: Stats,
}

#[derive(Default)]
struct Stats {
    window: Option<Instant>,
    frames: u32,
    empty: u32,
    copy_us: u64,
    rects: u32,
}

impl StreamData {
    /// Sample every frame inside the connect window, then ~1/s.
    fn wants_sample(&mut self, now: Instant) -> bool {
        if now.saturating_duration_since(self.connected_at) < CONNECT_WINDOW {
            return true;
        }
        if self
            .last_sample
            .is_none_or(|t| now.saturating_duration_since(t) >= Duration::from_secs(1))
        {
            self.last_sample = Some(now);
            return true;
        }
        false
    }

    fn log_stats(&mut self, now: Instant) {
        if !self.debug {
            return;
        }
        let start = *self.stats.window.get_or_insert(now);
        let elapsed = now.saturating_duration_since(start);
        if elapsed < Duration::from_secs(1) {
            return;
        }
        let s = std::mem::take(&mut self.stats);
        let n = s.frames.max(1) as u64;
        eprintln!(
            "[backend-kde] capture: {:.1} fps, {} empty, copy {:.2} ms avg, {:.1} damage rects avg",
            s.frames as f64 / elapsed.as_secs_f64(),
            s.empty,
            s.copy_us as f64 / n as f64 / 1000.0,
            s.rects as f64 / n as f64,
        );
        self.stats.window = Some(now);
    }
}

struct Active {
    stream: pw::stream::StreamRc,
    _listener: pw::stream::StreamListener<StreamData>,
}

/// Spawn the capture thread. Idle until it receives [`Command::Connect`].
pub fn spawn(
    frames: FrameTx,
    wake: Option<FrameWake>,
    events: EventTx,
) -> std::io::Result<(CommandTx, std::thread::JoinHandle<()>)> {
    let (tx, rx) = pw::channel::channel::<Command>();
    let handle = std::thread::Builder::new()
        .name("kde-pipewire".into())
        .spawn(move || {
            if let Err(e) = run(rx, frames, wake, events) {
                eprintln!("[backend-kde] capture thread failed: {e}");
            }
        })?;
    Ok((tx, handle))
}

fn run(
    rx: pw::channel::Receiver<Command>,
    frames: FrameTx,
    wake: Option<FrameWake>,
    events: EventTx,
) -> Result<(), pw::Error> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let active: Rc<RefCell<Option<Active>>> = Rc::new(RefCell::new(None));
    let debug = std::env::var_os("WAYRDP_DEBUG_CAPTURE").is_some();

    let ml = mainloop.clone();
    let active_cmd = active.clone();
    let _attached = rx.attach(mainloop.loop_(), move |cmd| match cmd {
        Command::Connect(node) => {
            if let Some(old) = active_cmd.borrow_mut().take() {
                let _ = old.stream.disconnect();
            }
            match connect(
                &core,
                node,
                frames.clone(),
                wake.clone(),
                events.clone(),
                debug,
            ) {
                Ok(a) => {
                    eprintln!("[backend-kde] capture: connecting to node {node}");
                    *active_cmd.borrow_mut() = Some(a);
                }
                Err(e) => {
                    eprintln!("[backend-kde] capture: connect to node {node} failed: {e}");
                    let _ = events.send(Event::StreamLost(Instant::now()));
                }
            }
        }
        Command::Disconnect => {
            if let Some(old) = active_cmd.borrow_mut().take() {
                let _ = old.stream.disconnect();
            }
        }
        Command::Shutdown => {
            if let Some(old) = active_cmd.borrow_mut().take() {
                let _ = old.stream.disconnect();
            }
            ml.quit();
        }
    });
    mainloop.run();
    Ok(())
}

fn connect(
    core: &pw::core::CoreRc,
    node: u32,
    frames: FrameTx,
    wake: Option<FrameWake>,
    events: EventTx,
    debug: bool,
) -> Result<Active, pw::Error> {
    let stream = pw::stream::StreamRc::new(
        core.clone(),
        "wayrdp-kde",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;
    let data = StreamData {
        format: Default::default(),
        frames,
        wake,
        events,
        connected_at: Instant::now(),
        last_sample: None,
        streaming: false,
        debug,
        stats: Stats::default(),
    };
    let listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|_, d, old, new| {
            eprintln!("[backend-kde] capture: stream {old:?} -> {new:?}");
            match new {
                pw::stream::StreamState::Streaming => d.streaming = true,
                pw::stream::StreamState::Error(_) | pw::stream::StreamState::Unconnected
                    if d.streaming =>
                {
                    d.streaming = false;
                    let _ = d.events.send(Event::StreamLost(Instant::now()));
                }
                _ => {}
            }
        })
        .param_changed(|stream, d, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            if d.format.parse(param).is_err() {
                return;
            }
            let size = d.format.size();
            eprintln!(
                "[backend-kde] capture: format {:?} {}x{}",
                d.format.format(),
                size.width,
                size.height
            );
            // An in-place mode change shows up here too; the next frames
            // simply carry the new size.
            d.connected_at = Instant::now();
            let params = buffer_params();
            let mut pods: Vec<&Pod> = params.iter().filter_map(|b| Pod::from_bytes(b)).collect();
            if let Err(e) = stream.update_params(&mut pods) {
                eprintln!("[backend-kde] capture: update_params failed: {e}");
            }
        })
        .process(process)
        .register()?;

    let format = enum_format();
    let mut params = [Pod::from_bytes(&format).expect("valid EnumFormat pod")];
    stream.connect(
        spa::utils::Direction::Input,
        Some(node),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;
    Ok(Active {
        stream,
        _listener: listener,
    })
}

fn process(stream: &pw::stream::Stream, d: &mut StreamData) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let now = Instant::now();
    if let Some(h) = buffer.find_meta::<spa::buffer::meta::MetaHeader>() {
        if h.flags()
            .contains(spa::buffer::meta::MetaHeaderFlags::CORRUPTED)
        {
            return;
        }
    }
    let regions: Vec<(i32, i32, u32, u32)> = buffer
        .find_meta::<spa::buffer::meta::MetaVideoDamage>()
        .map(|m| {
            m.iter()
                .take(16)
                .map(|r| {
                    let p = r.position();
                    let s = r.size();
                    (p.x, p.y, s.width, s.height)
                })
                .collect()
        })
        .unwrap_or_default();
    let size = d.format.size();
    let (width, height) = (size.width, size.height);
    let datas = buffer.datas_mut();
    let Some(data) = datas.first_mut() else {
        return;
    };
    let (offset, used, chunk_stride) = {
        let c = data.chunk();
        (c.offset() as usize, c.size() as usize, c.stride())
    };
    if used == 0 || width == 0 || height == 0 {
        d.stats.empty += 1;
        d.log_stats(now);
        return;
    }
    let stride = if chunk_stride > 0 {
        chunk_stride as usize
    } else {
        width as usize * 4
    };
    let Some(bytes) = data.data() else {
        return;
    };
    let len = stride * height as usize;
    let Some(src) = bytes.get(offset..offset + len) else {
        d.stats.empty += 1;
        return;
    };
    let t = Instant::now();
    let pixels: Box<[u8]> = src.into();
    d.stats.copy_us += t.elapsed().as_micros() as u64;
    let sample = if d.wants_sample(now) {
        Sample::of(sample_uniform(&pixels, width, height, stride))
    } else {
        Sample::Skipped
    };
    let damage = clip_damage(&regions, width, height);
    d.stats.frames += 1;
    d.stats.rects += damage.len() as u32;
    drop(buffer); // requeue before the send

    let frame = Frame {
        meta: FrameMeta {
            size: Size { width, height },
            stride,
            format: PixelFormat::Bgra32,
            damage,
            capture_stamp_ms: input_keymap::now_mono_ms_u32(),
        },
        pixels: Arc::new(pixels),
    };
    if d.frames.send(frame).is_err() {
        return; // peer gone; the session tears us down
    }
    if let Some(w) = &d.wake {
        w();
    }
    let _ = d.events.send(Event::Frame { sample, at: now });
    d.log_stats(now);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: i32, y: i32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn no_damage_means_full_frame() {
        assert_eq!(clip_damage(&[], 100, 50), vec![r(0, 0, 100, 50)]);
        assert_eq!(
            clip_damage(&[(0, 0, 0, 0)], 100, 50),
            vec![r(0, 0, 100, 50)]
        );
    }

    #[test]
    fn damage_is_clipped_and_stops_at_the_terminator() {
        let regions = [
            (90, 40, 20, 20),
            (-5, -5, 10, 10),
            (0, 0, 0, 0),
            (1, 1, 5, 5),
        ];
        assert_eq!(
            clip_damage(&regions, 100, 50),
            vec![r(90, 40, 10, 10), r(0, 0, 5, 5)]
        );
    }

    #[test]
    fn damage_entirely_outside_falls_back_to_full_frame() {
        assert_eq!(
            clip_damage(&[(200, 200, 5, 5)], 100, 50),
            vec![r(0, 0, 100, 50)]
        );
    }

    #[test]
    fn format_and_buffer_pods_serialize() {
        assert!(Pod::from_bytes(&enum_format()).is_some());
        for p in buffer_params() {
            assert!(Pod::from_bytes(&p).is_some());
        }
    }
}
