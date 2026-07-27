use std::collections::HashMap;
use std::error::Error;
use std::os::unix::io::AsRawFd;
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, AtomicU8, Ordering},
    Arc, Mutex,
};
use std::time::Duration;
use tracing::{debug, error, info, trace, warn};

use dbus::{
    arg::{OwnedFd, PropMap, RefArg, Variant},
    blocking::{Proxy, SyncConnection},
    message::{MatchRule, MessageType},
    Message,
};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::AppSink;

use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};

use hbb_common::{bail, config, platform::linux::CMD_SH, serde_json, tokio, ResultType};

use super::capturable::PixelProvider;
use super::capturable::{Capturable, Recorder};
use super::display::{clear_wayland_displays_cache, get_displays, Displays};
use super::remote_desktop_portal::OrgFreedesktopPortalRemoteDesktop as remote_desktop_portal;
use super::request_portal::OrgFreedesktopPortalRequestResponse;
use super::screencast_portal::OrgFreedesktopPortalScreenCast as screencast_portal;

lazy_static! {
    pub static ref RDP_SESSION_INFO: Mutex<Option<RdpSessionInfo>> = Mutex::new(None);
    static ref EXTRA_RDP_SESSION_INFO: Mutex<Vec<RdpSessionInfo>> = Mutex::new(Vec::new());
    static ref PIPEWIRE_PIPELINE_START_LOCK: Mutex<()> = Mutex::new(());
}

#[derive(Serialize, Deserialize)]
// For KDE Plasma only, because GNOME provides position info.
struct PipewireDisplayOffsetCache {
    // We need to compare the displays, because:
    // 1. On Archlinux KDE Plasma
    // 2. One display, and connect, remember share choice.
    // 3. Plug in another monitor.
    // 4. The portal will reuse the restore token, no new share choice dialog, but the share screen is different.
    //    The controlling side will see the new monitor.
    // All displays as one string for easy comparison
    // name1-x1-y1-width1-height1;name2-x2-y2-width2-height2;...
    display_key: String,
    restore_token: String,
    offsets: Vec<(i32, i32)>,
}

// KDE Plasma may not provide position info
static HAS_POSITION_ATTR: AtomicBool = AtomicBool::new(false);
static IS_SERVER_RUNNING: AtomicU8 = AtomicU8::new(0); // 0: uninitialized, 1:true, 2: false
static TRIED_ADDITIONAL_GRANTS: AtomicBool = AtomicBool::new(false);

impl PipewireDisplayOffsetCache {
    fn displays_to_key(displays: &Arc<Displays>) -> String {
        displays
            .displays
            .iter()
            .map(|d| format!("{}-{}-{}-{}-{}", d.name, d.x, d.y, d.width, d.height))
            .collect::<Vec<String>>()
            .join(";")
    }
}

// Shared teardown for both close paths. Callers must already hold, or not want,
// the RDP_SESSION_INFO lock: the lock order everywhere is RDP_SESSION_INFO then
// EXTRA_RDP_SESSION_INFO, and this keeps to it.
//
// Note we do NOT call `close_portal_session()` on the sessions being discarded
// here, unlike the duplicate-grant path in `build_hyprland_sessions()`. That is
// deliberate rather than an oversight: there we discard one grant and
// immediately request another for the same output, so the close has to be
// prompt and ordered before the next request. Here the whole connection is
// dropped and nothing re-requests, which is how upstream has always torn these
// down. Closing explicitly would add up to a 5s D-Bus timeout per session on
// the disconnect path for no observed benefit.
fn reset_session_state() {
    EXTRA_RDP_SESSION_INFO.lock().unwrap().clear();
    clear_wayland_displays_cache();
    HAS_POSITION_ATTR.store(false, Ordering::SeqCst);
    TRIED_ADDITIONAL_GRANTS.store(false, Ordering::SeqCst);
}

#[inline]
pub fn close_session() {
    let _ = RDP_SESSION_INFO.lock().unwrap().take();
    reset_session_state();
}

#[inline]
pub fn is_rdp_session_hold() -> bool {
    RDP_SESSION_INFO.lock().unwrap().is_some()
}

pub fn try_close_session() {
    let mut rdp_info = RDP_SESSION_INFO.lock().unwrap();
    let mut close = false;
    if let Some(rdp_info) = &*rdp_info {
        // If is server running and restore token is supported, there's no need to keep the session.
        if is_server_running() && rdp_info.is_support_restore_token {
            close = true;
        }
    }
    if close {
        *rdp_info = None;
        reset_session_state();
    }
}

#[inline]
pub fn set_server_running(is_running: bool) {
    IS_SERVER_RUNNING.store(if is_running { 1 } else { 2 }, Ordering::SeqCst);
}

pub struct RdpSessionInfo {
    pub conn: Arc<SyncConnection>,
    pub streams: Vec<PwStreamInfo>,
    pub fd: OwnedFd,
    pub session: dbus::Path<'static>,
    pub is_support_restore_token: bool,
    pub resolution: Arc<Mutex<Option<(usize, usize)>>>,
}
#[derive(Debug, Clone)]
pub struct PwStreamInfo {
    pub path: u64,
    source_type: u64,
    position: (i32, i32),
    size: (usize, usize),
    mapping_id: Option<String>,
}

impl PwStreamInfo {
    pub fn get_size(&self) -> (usize, usize) {
        self.size
    }

    pub fn get_position(&self) -> (i32, i32) {
        self.position
    }
}

#[derive(Debug)]
pub struct DBusError(String);

impl std::fmt::Display for DBusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self(s) = self;
        write!(f, "{}", s)
    }
}

impl Error for DBusError {}

#[derive(Debug)]
pub struct GStreamerError(String);

impl std::fmt::Display for GStreamerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self(s) = self;
        write!(f, "{}", s)
    }
}

impl Error for GStreamerError {}

#[derive(Clone)]
pub struct PipeWireCapturable {
    // connection needs to be kept alive for recording
    dbus_conn: Arc<SyncConnection>,
    fd: OwnedFd,
    path: u64,
    source_type: u64,
    crop: Option<(usize, usize, usize, usize)>,
    pub primary: bool,
    pub position: (i32, i32),
    pub logical_size: (usize, usize),
    pub physical_size: (usize, usize),
}

impl PipeWireCapturable {
    fn new(
        conn: Arc<SyncConnection>,
        fd: OwnedFd,
        resolution: Arc<Mutex<Option<(usize, usize)>>>,
        stream: &PwStreamInfo,
    ) -> Self {
        // Hyprland returns the selected monitor size in the portal stream metadata.
        // Avoid probing it with a temporary GStreamer pipeline, because multiple
        // PipeWire streams can become unstable when we start probe recorders
        // during every server-side display refresh/switch.
        let physical_size = if is_server_running() && is_hyprland_session() {
            stream.size
        } else {
            // alternative to get screen resolution as stream.size is not always correct ex: on fractional scaling
            // https://github.com/rustdesk/rustdesk/issues/6116#issuecomment-1817724244
            get_res(Self {
                dbus_conn: conn.clone(),
                fd: fd.clone(),
                path: stream.path,
                source_type: stream.source_type,
                crop: None,
                primary: false,
                position: stream.position,
                logical_size: stream.size,
                physical_size: (0, 0),
            })
            .unwrap_or(stream.size)
        };
        *resolution.lock().unwrap() = Some(physical_size);
        Self {
            dbus_conn: conn,
            fd,
            path: stream.path,
            source_type: stream.source_type,
            crop: None,
            primary: false,
            position: stream.position,
            logical_size: stream.size,
            physical_size,
        }
    }
}

impl std::fmt::Debug for PipeWireCapturable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PipeWireCapturable {{dbus: {}, fd: {}, path: {}, source_type: {}}}",
            self.dbus_conn.unique_name(),
            self.fd.as_raw_fd(),
            self.path,
            self.source_type
        )
    }
}

impl Capturable for PipeWireCapturable {
    fn name(&self) -> String {
        let type_str = match self.source_type {
            1 => "Desktop",
            2 => "Window",
            _ => "Unknow",
        };
        format!("Pipewire {}, path: {}", type_str, self.path)
    }

    fn geometry_relative(&self) -> Result<(f64, f64, f64, f64), Box<dyn Error>> {
        Ok((0.0, 0.0, 1.0, 1.0))
    }

    fn before_input(&mut self) -> Result<(), Box<dyn Error>> {
        Ok(())
    }

    fn recorder(&self, _capture_cursor: bool) -> Result<Box<dyn Recorder>, Box<dyn Error>> {
        Ok(Box::new(PipeWireRecorder::new(self.clone())?))
    }
}

fn desktop_bounds(
    displays: &[hbb_common::platform::linux::WaylandDisplayInfo],
    logical: bool,
) -> Option<(i32, i32, usize, usize)> {
    if displays.is_empty() {
        return None;
    }

    let min_x = displays.iter().map(|d| d.x).min()?;
    let min_y = displays.iter().map(|d| d.y).min()?;
    let display_size = |d: &hbb_common::platform::linux::WaylandDisplayInfo| {
        if logical {
            d.logical_size.unwrap_or((d.width, d.height))
        } else {
            (d.width, d.height)
        }
    };
    let max_x = displays
        .iter()
        .map(|d| {
            let (w, _) = display_size(d);
            d.x + w
        })
        .max()?;
    let max_y = displays
        .iter()
        .map(|d| {
            let (_, h) = display_size(d);
            d.y + h
        })
        .max()?;
    if max_x <= min_x || max_y <= min_y {
        return None;
    }

    Some((
        min_x,
        min_y,
        (max_x - min_x) as usize,
        (max_y - min_y) as usize,
    ))
}

fn split_workspace_capturable(
    capturable: PipeWireCapturable,
) -> Result<Vec<PipeWireCapturable>, PipeWireCapturable> {
    let displays = get_displays();
    if displays.displays.len() <= 1 || capturable.crop.is_some() {
        return Err(capturable);
    }

    let Some((min_x, min_y, desktop_w, desktop_h, logical_basis)) =
        desktop_bounds(&displays.displays, false)
            .map(|(x, y, w, h)| (x, y, w, h, false))
            .filter(|(_, _, w, h, _)| capturable.physical_size == (*w, *h))
            .or_else(|| {
                desktop_bounds(&displays.displays, true)
                    .map(|(x, y, w, h)| (x, y, w, h, true))
                    .filter(|(_, _, w, h, _)| capturable.physical_size == (*w, *h))
            })
    else {
        return Err(capturable);
    };

    let mut capturables = Vec::with_capacity(displays.displays.len());
    for wd in displays.displays.iter() {
        let x = wd.x - min_x;
        let y = wd.y - min_y;
        if x < 0 || y < 0 {
            return Err(capturable);
        }
        let (crop_w, crop_h) = if logical_basis {
            wd.logical_size.unwrap_or((wd.width, wd.height))
        } else {
            (wd.width, wd.height)
        };
        if crop_w <= 0 || crop_h <= 0 {
            return Err(capturable);
        }
        let crop = (x as usize, y as usize, crop_w as usize, crop_h as usize);
        if crop.0 + crop.2 > desktop_w || crop.1 + crop.3 > desktop_h {
            return Err(capturable);
        }

        let logical_size = wd
            .logical_size
            .map(|(w, h)| (w as usize, h as usize))
            .unwrap_or((wd.width as usize, wd.height as usize));
        let mut display_capturable = capturable.clone();
        display_capturable.crop = Some(crop);
        display_capturable.position = (wd.x, wd.y);
        display_capturable.logical_size = logical_size;
        display_capturable.physical_size = (crop.2, crop.3);
        capturables.push(display_capturable);
    }

    debug!(
        "Split single Wayland workspace stream {}x{} into {} monitor capturables.",
        desktop_w,
        desktop_h,
        capturables.len()
    );
    Ok(capturables)
}

fn capturables_from_session(rdp_info: &RdpSessionInfo) -> Vec<PipeWireCapturable> {
    rdp_info
        .streams
        .iter()
        .map(|s| {
            PipeWireCapturable::new(
                rdp_info.conn.clone(),
                rdp_info.fd.clone(),
                rdp_info.resolution.clone(),
                s,
            )
        })
        .collect()
}

fn log_capturables(label: &str, capturables: &[PipeWireCapturable]) {
    debug!(
        "{}: {} Wayland capturable stream(s): {:?}",
        label,
        capturables.len(),
        capturables
            .iter()
            .map(|c| (
                c.path.to_string(),
                c.position,
                c.logical_size,
                c.physical_size,
                c.crop
            ))
            .collect::<Vec<_>>()
    );
}

fn append_held_extra_grants(capturables: &mut Vec<PipeWireCapturable>) {
    let extra_sessions = EXTRA_RDP_SESSION_INFO.lock().unwrap();
    if extra_sessions.is_empty() {
        return;
    }

    for rdp_info in extra_sessions.iter() {
        capturables.extend(capturables_from_session(rdp_info));
    }
}

fn extend_with_additional_grants(capturables: &mut Vec<PipeWireCapturable>) {
    if !is_hyprland_session()
        || !is_server_running()
        || TRIED_ADDITIONAL_GRANTS.swap(true, Ordering::SeqCst)
    {
        return;
    }

    let compositor_display_count = get_displays().displays.len();
    if compositor_display_count <= capturables.len() {
        return;
    }

    warn!(
        "Wayland portal granted {} stream(s) for {} compositor display(s); requesting additional one-display grants.",
        capturables.len(),
        compositor_display_count
    );

    let mut extra_sessions = EXTRA_RDP_SESSION_INFO.lock().unwrap();
    while capturables.len() < compositor_display_count {
        let parts = match request_remote_desktop(false, None) {
            Ok(session) => session,
            Err(err) => {
                warn!(
                    "Stopped requesting additional Wayland display grants: {}",
                    err
                );
                break;
            }
        };
        if parts.2.is_empty() {
            warn!("Additional Wayland display grant returned no streams.");
            break;
        }

        let rdp_info = new_rdp_session(parts);
        let extra_capturables = capturables_from_session(&rdp_info);
        log_capturables("Additional Wayland portal grant", &extra_capturables);
        capturables.extend(extra_capturables);
        extra_sessions.push(rdp_info);
    }
}

fn get_res(capturable: PipeWireCapturable) -> Result<(usize, usize), Box<dyn Error>> {
    let rec = PipeWireRecorder::new(capturable)?;
    if let Some(sample) = rec
        .appsink
        .try_pull_sample(gst::ClockTime::from_mseconds(300))
    {
        let cap = sample
            .get_caps()
            .ok_or("Failed get caps")?
            .get_structure(0)
            .ok_or("Failed to get structure")?;
        let w: i32 = cap.get_value("width")?.get_some()?;
        let h: i32 = cap.get_value("height")?.get_some()?;
        let w = w as usize;
        let h = h as usize;
        Ok((w, h))
    } else {
        Err(Box::new(GStreamerError(
            "Error getting screen resolution".into(),
        )))
    }
}

pub struct PipeWireRecorder {
    buffer: Option<gst::MappedBuffer<gst::buffer::Readable>>,
    buffer_cropped: Vec<u8>,
    crop: Option<(usize, usize, usize, usize)>,
    pix_fmt: String,
    is_cropped: bool,
    node_id: u64,
    pipeline: gst::Pipeline,
    appsink: AppSink,
    width: usize,
    height: usize,
    saved_raw_data: Vec<u8>, // for faster compare and copy
}

impl PipeWireRecorder {
    pub fn new(capturable: PipeWireCapturable) -> ResultType<Self> {
        let node_id = capturable.path;
        let (pipeline, appsink) = {
            info!(
                "[gstreamer] PipeWire node {} waiting for serialized pipeline startup",
                node_id
            );
            let _startup_guard = PIPEWIRE_PIPELINE_START_LOCK
                .lock()
                .unwrap_or_else(|poisoned| {
                    warn!(
                        "[gstreamer] Recovering poisoned pipeline startup lock for PipeWire node {}",
                        node_id
                    );
                    poisoned.into_inner()
                });
            info!(
                "[gstreamer] PipeWire node {} pipeline startup begin (fd={})",
                node_id,
                capturable.fd.as_raw_fd()
            );

            let pipeline = gst::Pipeline::new(None);

            let src = gst::ElementFactory::make("pipewiresrc", None)?;
            src.set_property("fd", &capturable.fd.as_raw_fd())?;
            src.set_property("path", &format!("{}", node_id))?;
            src.set_property("keepalive_time", &1_000.as_raw_fd())?;

            // For some reason pipewire blocks on destruction of AppSink if this is not set to true,
            // see: https://gitlab.freedesktop.org/pipewire/pipewire/-/issues/982
            src.set_property("always-copy", &true)?;

            // COSMIC/Wayland fix: insert videoconvert between pipewiresrc and appsink.
            // xdg-desktop-portal-cosmic's modifier negotiation fails when the downstream
            // format set is too narrow (appsink only accepts BGRx/RGBx), producing
            // "no more output formats" / not-negotiated (-4). videoconvert accepts any
            // system-memory video/x-raw format, widening negotiation so the portal can
            // settle on a format it can deliver via its SHM path.
            let convert = gst::ElementFactory::make("videoconvert", None)?;

            let sink = gst::ElementFactory::make("appsink", None)?;
            sink.set_property("drop", &true)?;
            sink.set_property("max-buffers", &1u32)?;

            pipeline.add_many(&[&src, &convert, &sink])?;
            src.link(&convert)?;
            convert.link(&sink)?;

            let appsink = sink
                .dynamic_cast::<AppSink>()
                .map_err(|_| GStreamerError("Sink element is expected to be an appsink!".into()))?;
            let mut caps = gst::Caps::new_empty();
            caps.merge_structure(gst::structure::Structure::new(
                "video/x-raw",
                &[("format", &"BGRx")],
            ));
            caps.merge_structure(gst::structure::Structure::new(
                "video/x-raw",
                &[("format", &"RGBx")],
            ));
            appsink.set_caps(Some(&caps));

            // [Workaround]
            // Crash may occur if there are multiple pipelines started at the same time.
            // Serialize construction through PLAYING confirmation and the settling
            // delay, while allowing already-started pipelines to capture concurrently.
            info!(
                "[gstreamer] PipeWire node {} requesting pipeline state PLAYING",
                node_id
            );
            let set_state_result = pipeline.set_state(gst::State::Playing)?;
            info!(
                "[gstreamer] PipeWire node {} set_state(PLAYING) returned {:?}",
                node_id, set_state_result
            );

            // If `is_server_running()` is false, it means using remote_desktop_portal,
            // which does not use multiple streams, so no need to wait for state change.
            if is_server_running() {
                // Wait for the state change to actually complete before proceeding.
                // The 2000ms timeout for pipeline state change was chosen based on empirical testing.
                let state_change = pipeline.get_state(gst::ClockTime::from_mseconds(2000));
                match state_change {
                    (Ok(result), gst::State::Playing, pending) => {
                        info!(
                            "[gstreamer] PipeWire node {} reached PLAYING: result={:?}, pending={:?}",
                            node_id, result, pending
                        );
                    }
                    (result, state, pending) => {
                        warn!(
                            "[gstreamer] PipeWire node {} PLAYING transition incomplete: result={:?}, state={:?}, pending={:?}",
                            node_id, result, state, pending
                        );
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
            info!(
                "[gstreamer] PipeWire node {} serialized pipeline startup complete",
                node_id
            );

            (pipeline, appsink)
        };

        Ok(Self {
            node_id,
            pipeline,
            appsink,
            buffer: None,
            crop: capturable.crop,
            pix_fmt: "".into(),
            width: 0,
            height: 0,
            buffer_cropped: vec![],
            is_cropped: false,
            saved_raw_data: Vec::new(),
        })
    }
}

impl Recorder for PipeWireRecorder {
    fn capture(&mut self, timeout_ms: u64) -> Result<PixelProvider<'_>, Box<dyn Error>> {
        if let Some(sample) = self
            .appsink
            .try_pull_sample(gst::ClockTime::from_mseconds(timeout_ms))
        {
            let cap = sample
                .get_caps()
                .ok_or("Failed get caps")?
                .get_structure(0)
                .ok_or("Failed to get structure")?;
            let w: i32 = cap.get_value("width")?.get_some()?;
            let h: i32 = cap.get_value("height")?.get_some()?;
            let w = w as usize;
            let h = h as usize;
            self.pix_fmt = cap
                .get::<&str>("format")?
                .ok_or("Failed to get pixel format")?
                .to_string();

            let buf = sample
                .get_buffer_owned()
                .ok_or_else(|| GStreamerError("Failed to get owned buffer.".into()))?;
            let mut crop = buf
                .get_meta::<gstreamer_video::VideoCropMeta>()
                .map(|m| m.get_rect());
            // only crop if necessary
            if Some((0, 0, w as u32, h as u32)) == crop {
                crop = None;
            }
            let crop = self
                .crop
                .map(|(x, y, w, h)| (x as u32, y as u32, w as u32, h as u32))
                .or(crop);
            let buf = buf
                .into_mapped_buffer_readable()
                .map_err(|_| GStreamerError("Failed to map buffer.".into()))?;
            if let Err(..) = crate::would_block_if_equal(&mut self.saved_raw_data, buf.as_slice()) {
                return Ok(PixelProvider::NONE);
            }
            let buf_size = buf.get_size();
            // BGRx is 4 bytes per pixel
            if buf_size != (w * h * 4) {
                // for some reason the width and height of the caps do not guarantee correct buffer
                // size, so ignore those buffers, see:
                // https://gitlab.freedesktop.org/pipewire/pipewire/-/issues/985
                trace!(
                    "Size of mapped buffer: {} does NOT match size of capturable {}x{}@BGRx, \
                    dropping it!",
                    buf_size,
                    w,
                    h
                );
            } else {
                // Copy region specified by crop into self.buffer_cropped
                // TODO: Figure out if ffmpeg provides a zero copy alternative
                if let Some((x_off, y_off, w_crop, h_crop)) = crop {
                    let x_off = x_off as usize;
                    let y_off = y_off as usize;
                    let w_crop = w_crop as usize;
                    let h_crop = h_crop as usize;
                    if x_off + w_crop > w || y_off + h_crop > h {
                        return Err(Box::new(GStreamerError(format!(
                            "Crop {:?} exceeds PipeWire frame size {}x{}",
                            (x_off, y_off, w_crop, h_crop),
                            w,
                            h
                        ))));
                    }
                    self.buffer_cropped.clear();
                    let data = buf.as_slice();
                    // BGRx is 4 bytes per pixel
                    self.buffer_cropped.reserve(w_crop * h_crop * 4);
                    for y in y_off..(y_off + h_crop) {
                        let i = 4 * (w * y + x_off);
                        self.buffer_cropped.extend(&data[i..i + 4 * w_crop]);
                    }
                    self.width = w_crop;
                    self.height = h_crop;
                } else {
                    self.width = w;
                    self.height = h;
                }
                self.is_cropped = crop.is_some();
                self.buffer = Some(buf);
            }
        } else {
            return Ok(PixelProvider::NONE);
        }
        if self.buffer.is_none() {
            return Err(Box::new(GStreamerError("No buffer available!".into())));
        }
        let buf = if self.is_cropped {
            self.buffer_cropped.as_slice()
        } else {
            self.buffer
                .as_ref()
                .ok_or("Failed to get buffer as ref")?
                .as_slice()
        };
        match self.pix_fmt.as_str() {
            "BGRx" => Ok(PixelProvider::BGR0(self.width, self.height, buf)),
            "RGBx" => Ok(PixelProvider::RGB0(self.width, self.height, buf)),
            _ => Err(Box::new(GStreamerError(format!(
                "Unreachable! Unknown pix_fmt, {}",
                &self.pix_fmt
            )))),
        }
    }
}

impl Drop for PipeWireRecorder {
    fn drop(&mut self) {
        info!(
            "[gstreamer] PipeWire node {} requesting pipeline state NULL",
            self.node_id
        );
        if let Err(err) = self.pipeline.set_state(gst::State::Null) {
            warn!(
                "[gstreamer] PipeWire node {} failed to request NULL state: {}",
                self.node_id, err
            );
        }
        // Wait for state change to complete to avoid races during PipeWire teardown.
        let (result, state, pending) = self.pipeline.get_state(gst::ClockTime::from_mseconds(2000));
        info!(
            "[gstreamer] PipeWire node {} stopped: result={:?}, state={:?}, pending={:?}",
            self.node_id, result, state, pending
        );
    }
}

fn handle_response<F>(
    conn: &SyncConnection,
    path: dbus::Path<'static>,
    mut f: F,
    failure_out: Arc<AtomicBool>,
) -> Result<dbus::channel::Token, dbus::Error>
where
    F: FnMut(
            OrgFreedesktopPortalRequestResponse,
            &SyncConnection,
            &Message,
        ) -> Result<(), Box<dyn Error>>
        + Send
        + Sync
        + 'static,
{
    let mut m = MatchRule::new();
    m.path = Some(path);
    m.msg_type = Some(MessageType::Signal);
    m.sender = Some("org.freedesktop.portal.Desktop".into());
    m.interface = Some("org.freedesktop.portal.Request".into());
    conn.add_match(m, move |r: OrgFreedesktopPortalRequestResponse, c, m| {
        debug!("Response from DBus: response: {:?}, message: {:?}", r, m);
        match r.response {
            0 => {}
            1 => {
                warn!("DBus response: User cancelled interaction.");
                failure_out.store(true, Ordering::SeqCst);
                return true;
            }
            c => {
                warn!("DBus response: Unknown error, code: {}.", c);
                failure_out.store(true, Ordering::SeqCst);
                return true;
            }
        }
        if let Err(err) = f(r, c, m) {
            warn!("Error requesting screen capture via dbus: {}", err);
            failure_out.store(true, Ordering::SeqCst);
        }
        true
    })
}

fn get_sender_normalized(conn: &SyncConnection) -> String {
    conn.unique_name().trim_start_matches(':').replace('.', "_")
}

fn get_request_path(
    conn: &SyncConnection,
    handle_token: &str,
) -> Result<dbus::Path<'static>, dbus::Error> {
    dbus::Path::new(format!(
        "/org/freedesktop/portal/desktop/request/{}/{}",
        get_sender_normalized(conn),
        handle_token
    ))
    .map_err(|_| dbus::Error::new_failed("Failed to construct portal request path"))
}

pub fn get_portal(conn: &SyncConnection) -> Proxy<&SyncConnection> {
    conn.with_proxy(
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        // 30s timeout: portal method calls (CreateSession, SelectSources, Start) must
        // return their request-path reply promptly per spec, but some compositors
        // (e.g. xdg-desktop-portal-hyprland) can take longer on first call due to
        // initialization overhead. The 1s default causes spurious NoReply errors.
        Duration::from_secs(30),
    )
}

fn streams_from_response(response: OrgFreedesktopPortalRequestResponse) -> Vec<PwStreamInfo> {
    (move || {
        Some(
            response
                .results
                .get("streams")?
                .as_iter()?
                .next()?
                .as_iter()?
                .filter_map(|stream| {
                    let mut itr = stream.as_iter()?;
                    let path = itr.next()?.as_u64()?;
                    let (keys, values): (Vec<(usize, &dyn RefArg)>, Vec<(usize, &dyn RefArg)>) =
                        itr.next()?
                            .as_iter()?
                            .enumerate()
                            .partition(|(i, _)| i % 2 == 0);
                    let attributes = keys
                        .iter()
                        .filter_map(|(_, key)| Some(key.as_str()?.to_owned()))
                        .zip(
                            values
                                .iter()
                                .map(|(_, arg)| *arg)
                                .collect::<Vec<&dyn RefArg>>(),
                        )
                        .collect::<HashMap<String, &dyn RefArg>>();
                    let mut info = PwStreamInfo {
                        path,
                        source_type: attributes
                            .get("source_type")
                            .map_or(Some(0), |v| v.as_u64())?,
                        position: (0, 0),
                        size: (0, 0),
                        mapping_id: attributes
                            .get("mapping_id")
                            .and_then(|v| v.as_str())
                            .map(str::to_owned),
                    };
                    let v = attributes
                        .get("size")?
                        .as_iter()?
                        .filter_map(|v| {
                            Some(
                                v.as_iter()?
                                    .map(|x| x.as_i64().unwrap_or(0))
                                    .collect::<Vec<i64>>(),
                            )
                        })
                        .next();
                    if let Some(v) = v {
                        if v.len() == 2 {
                            info.size.0 = v[0] as _;
                            info.size.1 = v[1] as _;
                        }
                    }
                    if let Some(pos) = attributes.get("position") {
                        let v = pos
                            .as_iter()?
                            .filter_map(|v| {
                                Some(
                                    v.as_iter()?
                                        .map(|x| x.as_i64().unwrap_or(0))
                                        .collect::<Vec<i64>>(),
                                )
                            })
                            .next();
                        if let Some(v) = v {
                            if v.len() == 2 {
                                info.position.0 = v[0] as _;
                                info.position.1 = v[1] as _;
                                HAS_POSITION_ATTR.store(true, Ordering::SeqCst);
                            }
                        }
                    }
                    Some(info)
                })
                .collect::<Vec<PwStreamInfo>>(),
        )
    })()
    .unwrap_or_default()
}

static mut INIT: bool = false;
const RESTORE_TOKEN: &str = "restore_token";
const RESTORE_TOKEN_CONF_KEY: &str = "wayland-restore-token";
// Hyprland's portal grants a single monitor per session, so one restore token
// (RESTORE_TOKEN_CONF_KEY) cannot cover a multi-monitor setup. Store a
// per-monitor map `{ monitor_name: restore_token }` as JSON under this key so
// each granted monitor can be restored silently after a RustDesk restart.
const RESTORE_TOKENS_CONF_KEY: &str = "wayland-restore-tokens";
const PIPEWIRE_DISPLAY_OFFSET_CONF_KEY: &str = "wayland-pipewire-display-offset";

pub fn is_hyprland_session() -> bool {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        return true;
    }

    if std::env::var("XDG_CURRENT_DESKTOP")
        .map(|desktop| desktop.to_ascii_lowercase().contains("hyprland"))
        .unwrap_or(false)
    {
        return true;
    }

    hbb_common::platform::linux::detect_hyprland_session()
}

fn should_use_restore_token(is_support_restore_token: bool) -> bool {
    // Restore tokens are now used on Hyprland too. They are stored per-monitor
    // (see RESTORE_TOKENS_CONF_KEY) because the portal grants one monitor per
    // session. Requires xdph with `screencopy:allow_token_by_default = true`
    // to actually skip the picker on restore.
    is_support_restore_token
}

fn clear_restore_token_state() {
    config::LocalConfig::set_option(RESTORE_TOKEN_CONF_KEY.to_owned(), "".to_owned());
    config::LocalConfig::set_option(RESTORE_TOKENS_CONF_KEY.to_owned(), "".to_owned());
    config::LocalConfig::set_option(PIPEWIRE_DISPLAY_OFFSET_CONF_KEY.to_owned(), "".to_owned());
}

// Per-monitor restore token storage (Hyprland). Keyed by compositor monitor
// name, e.g. "DP-3" / "HDMI-A-1".
fn load_restore_tokens() -> HashMap<String, String> {
    let raw = config::LocalConfig::get_option(RESTORE_TOKENS_CONF_KEY);
    if raw.is_empty() {
        return HashMap::new();
    }
    serde_json::from_str(&raw).unwrap_or_default()
}

fn save_restore_token_for(monitor: &str, token: &str) {
    if monitor.is_empty() || token.is_empty() {
        return;
    }
    let mut map = load_restore_tokens();
    let legacy = config::LocalConfig::get_option(RESTORE_TOKEN_CONF_KEY);
    let legacy_was_mapped_to_monitor = map
        .get(monitor)
        .map(|mapped| mapped == &legacy)
        .unwrap_or(false);
    if map.get(monitor).map(|t| t == token).unwrap_or(false) {
        if legacy_was_mapped_to_monitor {
            config::LocalConfig::set_option(RESTORE_TOKEN_CONF_KEY.to_owned(), "".to_owned());
            debug!(
                "Cleared legacy Wayland restore token after confirming the per-monitor token for {}.",
                monitor
            );
        }
        return;
    }
    map.insert(monitor.to_owned(), token.to_owned());
    match serde_json::to_string(&map) {
        Ok(serialized) => {
            config::LocalConfig::set_option(RESTORE_TOKENS_CONF_KEY.to_owned(), serialized);
            if legacy_was_mapped_to_monitor || legacy == token {
                config::LocalConfig::set_option(RESTORE_TOKEN_CONF_KEY.to_owned(), "".to_owned());
                debug!(
                    "Migrated legacy Wayland restore token to the per-monitor entry for {}.",
                    monitor
                );
            }
        }
        Err(err) => warn!("Failed to serialize Wayland restore tokens: {}", err),
    }
}

// Identify which compositor monitor a freshly granted stream belongs to.
// Hyprland per-monitor portal streams frequently report position (0, 0), so we
// prefer the portal's compositor-provided mapping_id. Keep physical resolution
// matching as a fallback for portal backends that do not provide mapping_id.
fn monitor_name_for_streams(streams: &[PwStreamInfo]) -> Option<String> {
    let stream = streams.first()?;
    let displays = get_displays();
    if let Some(mapping_id) = stream.mapping_id.as_deref() {
        if let Some(display) = displays.displays.iter().find(|d| d.name == mapping_id) {
            return Some(display.name.clone());
        }
        warn!(
            "Wayland stream mapping_id {:?} did not match a compositor monitor; falling back to resolution matching.",
            mapping_id
        );
    }

    let (sw, sh) = stream.size;
    displays
        .displays
        .iter()
        .find(|d| d.width as usize == sw && d.height as usize == sh)
        .map(|d| d.name.clone())
}

fn stamp_hyprland_stream_positions(streams: &mut [PwStreamInfo]) {
    if !is_hyprland_session() {
        return;
    }

    let displays = get_displays();
    for stream in streams {
        let Some(mapping_id) = stream.mapping_id.as_deref() else {
            continue;
        };
        let Some(display) = displays
            .displays
            .iter()
            .find(|display| display.name == mapping_id)
        else {
            warn!(
                "Wayland stream mapping_id {:?} did not match a compositor monitor; keeping portal position {:?}.",
                mapping_id, stream.position
            );
            continue;
        };

        let portal_position = stream.position;
        stream.position = (display.x, display.y);
        info!(
            "Stamped Hyprland stream node={} mapping_id={:?} position {:?} from compositor layout (portal position {:?}).",
            stream.path, mapping_id, stream.position, portal_position
        );
    }
}

pub fn get_available_cursor_modes() -> Result<u32, dbus::Error> {
    let conn = SyncConnection::new_session()?;
    let portal = get_portal(&conn);
    portal.available_cursor_modes()
}

// mostly inspired by https://gitlab.gnome.org/-/snippets/39
//
// `restore_token`: when set, ask the portal to restore a previously granted
// source (per-monitor on Hyprland). `None` falls back to the legacy single
// restore token (non-Hyprland) and otherwise prompts the user.
pub fn request_remote_desktop(
    capture_cursor: bool,
    restore_token: Option<String>,
) -> ResultType<(
    SyncConnection,
    OwnedFd,
    Vec<PwStreamInfo>,
    dbus::Path<'static>,
    bool,
)> {
    unsafe {
        if !INIT {
            gstreamer::init()?;
            INIT = true;
        }
    }
    let conn = SyncConnection::new_session()?;
    let portal = get_portal(&conn);
    let mut args: PropMap = HashMap::new();
    let fd: Arc<Mutex<Option<OwnedFd>>> = Arc::new(Mutex::new(None));
    let fd_res = fd.clone();
    let streams: Arc<Mutex<Vec<PwStreamInfo>>> = Arc::new(Mutex::new(Vec::new()));
    let streams_res = streams.clone();
    let failure = Arc::new(AtomicBool::new(false));
    let failure_res = failure.clone();
    let session: Arc<Mutex<Option<dbus::Path>>> = Arc::new(Mutex::new(None));
    let session_res = session.clone();
    let create_session_handle_token = "u1";
    args.insert(
        "session_handle_token".to_string(),
        Variant(Box::new(create_session_handle_token.to_string())),
    );
    args.insert(
        "handle_token".to_string(),
        Variant(Box::new(create_session_handle_token.to_string())),
    );

    let mut is_support_restore_token = false;
    if let Ok(version) = screencast_portal::version(&portal) {
        if version >= 4 {
            is_support_restore_token = true;
        }
    }
    if is_server_running() && !should_use_restore_token(is_support_restore_token) {
        debug!("Disabling Wayland restore token persistence for current session.");
        clear_restore_token_state();
    }

    // The following code may be improved.
    // https://flatpak.github.io/xdg-desktop-portal/#:~:text=To%20avoid%20a%20race%20condition
    // To avoid a race condition
    // between the caller subscribing to the signal after receiving the reply for the method call and the signal getting emitted,
    // a convention for Request object paths has been established that allows
    // the caller to subscribe to the signal before making the method call.
    handle_response(
        &conn,
        get_request_path(&conn, create_session_handle_token)?,
        on_create_session_response(
            fd.clone(),
            streams.clone(),
            session.clone(),
            failure.clone(),
            is_support_restore_token,
            capture_cursor,
            restore_token.clone(),
        ),
        failure_res.clone(),
    )?;
    if is_server_running() {
        let _ = screencast_portal::create_session(&portal, args)?;
    } else {
        let _ = remote_desktop_portal::create_session(&portal, args)?;
    }

    // wait 3 minutes for user interaction
    for _ in 0..1800 {
        conn.process(Duration::from_millis(100))?;
        // Once we got a file descriptor we are done!
        if fd_res.lock().unwrap().is_some() {
            break;
        }

        if failure_res.load(Ordering::SeqCst) {
            break;
        }
    }
    let fd_res = fd_res.lock().unwrap();
    let streams_res = streams_res.lock().unwrap();
    let session_res = session_res.lock().unwrap();

    if let Some(fd_res) = fd_res.clone() {
        if let Some(session) = session_res.clone() {
            if !streams_res.is_empty() {
                return Ok((
                    conn,
                    fd_res,
                    streams_res.clone(),
                    session,
                    is_support_restore_token,
                ));
            }
        }
    }
    bail!("Failed to obtain screen capture. You may need to upgrade the PipeWire library for better compatibility. Please check https://github.com/rustdesk/rustdesk/issues/8600#issuecomment-2254720954 for more details.")
}

fn on_create_session_response(
    fd: Arc<Mutex<Option<OwnedFd>>>,
    streams: Arc<Mutex<Vec<PwStreamInfo>>>,
    session: Arc<Mutex<Option<dbus::Path<'static>>>>,
    failure: Arc<AtomicBool>,
    is_support_restore_token: bool,
    capture_cursor: bool,
    restore_token: Option<String>,
) -> impl Fn(
    OrgFreedesktopPortalRequestResponse,
    &SyncConnection,
    &dbus::Message,
) -> Result<(), Box<dyn Error>> {
    move |r: OrgFreedesktopPortalRequestResponse, c, _| {
        let ses: dbus::Path = r
            .results
            .get("session_handle")
            .ok_or_else(|| {
                DBusError(format!(
                    "Failed to obtain session_handle from response: {:?}",
                    r
                ))
            })?
            .as_str()
            .ok_or_else(|| DBusError("Failed to convert session_handle to string.".into()))?
            .to_string()
            .into();

        let mut session = match session.lock() {
            Ok(session) => session,
            Err(_) => return Err(Box::new(DBusError("Failed to lock session.".into()))),
        };
        session.replace(ses.clone());

        let portal = get_portal(c);
        let mut args: PropMap = HashMap::new();
        // See `is_server_running()` to understand the following code.
        if is_server_running() {
            let is_hyprland = is_hyprland_session();
            let select_sources_handle_token = "u3";
            let mut restore_token_source = "unsupported";
            if should_use_restore_token(is_support_restore_token) {
                // Prefer the explicitly requested token (per-monitor on Hyprland);
                // otherwise fall back to the legacy single token (non-Hyprland).
                let explicit_restore_token = restore_token.clone().filter(|t| !t.is_empty());
                let (restore_token, source) = if let Some(token) = explicit_restore_token {
                    (Some(token), "per-monitor")
                } else if !is_hyprland {
                    let t = config::LocalConfig::get_option(RESTORE_TOKEN_CONF_KEY);
                    if t.is_empty() {
                        (None, "none")
                    } else {
                        (Some(t), "legacy")
                    }
                } else {
                    (None, "none")
                };
                restore_token_source = source;
                if let Some(restore_token) = restore_token {
                    args.insert(RESTORE_TOKEN.to_string(), Variant(Box::new(restore_token)));
                }
                // persist_mode may be configured by the user.
                args.insert("persist_mode".to_string(), Variant(Box::new(2u32)));
            }
            info!(
                "Wayland portal restore-token source for session {:?}: {}",
                ses, restore_token_source
            );
            args.insert(
                "handle_token".to_string(),
                Variant(Box::new(select_sources_handle_token.to_string())),
            );
            // https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html
            // Hyprland grants one monitor per portal session, and RustDesk creates
            // one session per compositor monitor. Keep multi-selection enabled on
            // other desktops, where one session may legitimately return many streams.
            args.insert("multiple".into(), Variant(Box::new(!is_hyprland)));
            args.insert("types".into(), Variant(Box::new(1u32))); //| 2u32)));

            if capture_cursor {
                get_available_cursor_modes().ok().map(|modes| {
                    if modes & 0x2 != 0 {
                        args.insert("cursor_mode".to_string(), Variant(Box::new(2u32)));
                    }
                });
            }

            handle_response(
                c,
                get_request_path(c, select_sources_handle_token)?,
                on_select_sources_response(
                    fd.clone(),
                    streams.clone(),
                    failure.clone(),
                    ses.clone(),
                    is_support_restore_token,
                ),
                failure.clone(),
            )?;
            let _ = portal.select_sources(ses.clone(), args)?;
        } else {
            // TODO: support persist_mode for remote_desktop_portal
            // https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html

            let select_devices_handle_token = "u2";
            args.insert(
                "handle_token".to_string(),
                Variant(Box::new(select_devices_handle_token.to_string())),
            );
            args.insert("types".to_string(), Variant(Box::new(7u32)));

            handle_response(
                c,
                get_request_path(c, select_devices_handle_token)?,
                on_select_devices_response(
                    fd.clone(),
                    streams.clone(),
                    failure.clone(),
                    ses.clone(),
                    is_support_restore_token,
                ),
                failure.clone(),
            )?;
            let _ = portal.select_devices(ses.clone(), args)?;
        }

        Ok(())
    }
}

fn on_select_devices_response(
    fd: Arc<Mutex<Option<OwnedFd>>>,
    streams: Arc<Mutex<Vec<PwStreamInfo>>>,
    failure: Arc<AtomicBool>,
    session: dbus::Path<'static>,
    is_support_restore_token: bool,
) -> impl Fn(
    OrgFreedesktopPortalRequestResponse,
    &SyncConnection,
    &dbus::Message,
) -> Result<(), Box<dyn Error>> {
    move |_: OrgFreedesktopPortalRequestResponse, c, _| {
        let portal = get_portal(c);
        let mut args: PropMap = HashMap::new();
        let select_sources_handle_token = "u3";
        args.insert(
            "handle_token".to_string(),
            Variant(Box::new(select_sources_handle_token.to_string())),
        );
        // https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html
        if is_server_running() {
            args.insert("multiple".into(), Variant(Box::new(true)));
        }
        args.insert("types".into(), Variant(Box::new(1u32))); //| 2u32)));

        let session = session.clone();
        handle_response(
            c,
            get_request_path(c, select_sources_handle_token)?,
            on_select_sources_response(
                fd.clone(),
                streams.clone(),
                failure.clone(),
                session.clone(),
                is_support_restore_token,
            ),
            failure.clone(),
        )?;
        let _ = portal.select_sources(session.clone(), args)?;

        Ok(())
    }
}

fn on_select_sources_response(
    fd: Arc<Mutex<Option<OwnedFd>>>,
    streams: Arc<Mutex<Vec<PwStreamInfo>>>,
    failure: Arc<AtomicBool>,
    session: dbus::Path<'static>,
    is_support_restore_token: bool,
) -> impl Fn(
    OrgFreedesktopPortalRequestResponse,
    &SyncConnection,
    &dbus::Message,
) -> Result<(), Box<dyn Error>> {
    move |_: OrgFreedesktopPortalRequestResponse, c, _| {
        let portal = get_portal(c);
        let mut args: PropMap = HashMap::new();
        let handle_token = "u4";
        args.insert(
            "handle_token".to_string(),
            Variant(Box::new(handle_token.to_string())),
        );

        // Pre-subscribe to the Start response BEFORE calling portal.start() to avoid
        // a race condition with fast compositors like Hyprland that respond immediately
        // (before we can register the match rule). The portal spec guarantees the
        // response path is predictable:
        // /org/freedesktop/portal/desktop/request/{sender_normalized}/{handle_token}
        handle_response(
            c,
            get_request_path(c, handle_token)?,
            on_start_response(
                fd.clone(),
                streams.clone(),
                session.clone(),
                is_support_restore_token,
            ),
            failure.clone(),
        )?;

        // Call Start AFTER subscribing to avoid missing the response signal on
        // compositors (e.g. Hyprland) that reply before we can register the match rule.
        if is_server_running() {
            screencast_portal::start(&portal, session.clone(), "", args)?;
        } else {
            remote_desktop_portal::start(&portal, session.clone(), "", args)?;
        }

        Ok(())
    }
}

fn on_start_response(
    fd: Arc<Mutex<Option<OwnedFd>>>,
    streams: Arc<Mutex<Vec<PwStreamInfo>>>,
    session: dbus::Path<'static>,
    is_support_restore_token: bool,
) -> impl Fn(
    OrgFreedesktopPortalRequestResponse,
    &SyncConnection,
    &dbus::Message,
) -> Result<(), Box<dyn Error>> {
    move |r: OrgFreedesktopPortalRequestResponse, c, _| {
        let portal = get_portal(c);
        // See `is_server_running()` to understand the following code.
        // Extract the restore token before `r` is consumed below; persist it
        // after the streams are parsed so we can key it to the granted monitor.
        let response_restore_token = if is_server_running() {
            r.results
                .get(RESTORE_TOKEN)
                .and_then(|t| t.as_str())
                .map(|s| s.to_owned())
        } else {
            None
        };

        let mut response_streams = streams_from_response(r);
        for stream in &response_streams {
            info!(
                "Wayland portal grant: node={}, mapping_id={:?}, size={:?}, position={:?}",
                stream.path, stream.mapping_id, stream.size, stream.position
            );
        }
        stamp_hyprland_stream_positions(&mut response_streams);
        debug!(
            "Portal start response returned {} stream(s): {:?}",
            response_streams.len(),
            response_streams
                .iter()
                .map(|stream| (
                    stream.path.to_string(),
                    stream.mapping_id.as_deref(),
                    stream.size,
                    stream.position
                ))
                .collect::<Vec<_>>()
        );

        if is_server_running() && should_use_restore_token(is_support_restore_token) {
            if is_hyprland_session() {
                // Hyprland: store the token per monitor so each display can be
                // restored independently after a restart.
                if let Some(token) = &response_restore_token {
                    if let Some(name) = monitor_name_for_streams(&response_streams) {
                        save_restore_token_for(&name, token);
                    } else {
                        warn!("Could not match granted Wayland stream to a monitor; restore token not persisted.");
                    }
                }
            } else if let Some(token) = &response_restore_token {
                config::LocalConfig::set_option(RESTORE_TOKEN_CONF_KEY.to_owned(), token.clone());
            } else {
                clear_restore_token_state();
            }
        }

        streams
            .clone()
            .lock()
            .unwrap()
            .append(&mut response_streams);
        fd.clone()
            .lock()
            .unwrap()
            .replace(portal.open_pipe_wire_remote(session.clone(), HashMap::new())?);

        Ok(())
    }
}

fn new_rdp_session(
    parts: (
        SyncConnection,
        OwnedFd,
        Vec<PwStreamInfo>,
        dbus::Path<'static>,
        bool,
    ),
) -> RdpSessionInfo {
    let (conn, fd, streams, session, is_support_restore_token) = parts;
    RdpSessionInfo {
        conn: Arc::new(conn),
        streams,
        fd,
        session,
        is_support_restore_token,
        resolution: Arc::new(Mutex::new(None)),
    }
}

fn close_portal_session(
    conn: &SyncConnection,
    session: dbus::Path<'static>,
) -> Result<(), dbus::Error> {
    let proxy = conn.with_proxy(
        "org.freedesktop.portal.Desktop",
        session,
        Duration::from_secs(5),
    );
    proxy.method_call("org.freedesktop.portal.Session", "Close", ())
}

// Build one capture session per compositor monitor on Hyprland. Saved
// per-monitor restore tokens are reused so the portal can restore them without
// a picker; monitors without a token (first-ever run, newly attached) prompt
// once and are then remembered. Returns the primary session plus any extras.
fn build_hyprland_sessions() -> ResultType<(RdpSessionInfo, Vec<RdpSessionInfo>)> {
    use std::collections::HashSet;
    const MAX_GRANT_ATTEMPTS_PER_MONITOR: usize = 2;

    let monitors = get_displays().displays.clone();
    if monitors.is_empty() {
        bail!("No Wayland displays to capture");
    }
    let saved = load_restore_tokens();
    let mut sessions: Vec<RdpSessionInfo> = Vec::new();
    let mut covered: HashSet<String> = HashSet::new();

    // Drive the loop off which monitors are still missing, not off the monitor
    // list itself. The portal decides which monitor it hands back, so a grant
    // that does not match the request must not consume that monitor's turn --
    // otherwise picking out of order (or a stale token restoring the wrong
    // output) leaves a monitor with no session at all.
    let mut token_tried: HashSet<String> = HashSet::new();
    let max_attempts = monitors.len() * MAX_GRANT_ATTEMPTS_PER_MONITOR;

    for attempt in 1..=max_attempts {
        let Some(requested) = monitors.iter().find(|m| !covered.contains(&m.name)) else {
            break;
        };
        let requested_name = requested.name.clone();
        // Offer a saved token only once per monitor. A stale token that restores
        // some other output would otherwise be replayed on every attempt.
        let token = if token_tried.insert(requested_name.clone()) {
            saved.get(&requested_name).cloned()
        } else {
            None
        };

        let parts = match request_remote_desktop(false, token) {
            Ok(parts) => parts,
            Err(err) => {
                warn!(
                    "Stopped requesting Hyprland monitor grants after {} session(s): {}",
                    sessions.len(),
                    err
                );
                break;
            }
        };
        if parts.2.is_empty() {
            warn!("Hyprland monitor grant returned no streams; stopping.");
            break;
        }

        // Record the monitor the portal actually granted, which need not be the
        // one requested.
        let granted =
            monitor_name_for_streams(&parts.2).unwrap_or_else(|| requested_name.clone());
        if !covered.insert(granted.clone()) {
            warn!(
                "Hyprland portal returned already-covered monitor {} while requesting {}; duplicate grant discarded (attempt {}/{}).",
                granted, requested_name, attempt, max_attempts
            );
            if let Err(err) = close_portal_session(&parts.0, parts.3.clone()) {
                warn!(
                    "Failed to close duplicate Hyprland portal session {:?}: {}",
                    parts.3, err
                );
            }
            continue;
        }
        if granted != requested_name {
            info!(
                "Hyprland portal granted {} while {} was requested; keeping it and requesting the remainder.",
                granted, requested_name
            );
        }
        sessions.push(new_rdp_session(parts));
    }

    let uncovered = monitors
        .iter()
        .filter(|monitor| !covered.contains(&monitor.name))
        .map(|monitor| monitor.name.as_str())
        .collect::<Vec<_>>();
    if !uncovered.is_empty() {
        warn!(
            "Hyprland capture sessions do not cover monitor(s): {}",
            uncovered.join(", ")
        );
    }

    if sessions.is_empty() {
        bail!("Failed to obtain any Hyprland capture session");
    }
    let primary = sessions.remove(0);
    Ok((primary, sessions))
}

pub fn get_capturables() -> Result<Vec<PipeWireCapturable>, Box<dyn Error>> {
    let mut rdp_connection = match RDP_SESSION_INFO.lock() {
        Ok(conn) => conn,
        Err(err) => return Err(Box::new(err)),
    };

    if rdp_connection.is_none() {
        if is_server_running() && is_hyprland_session() {
            // Hyprland grants one monitor per session. Build a session for every
            // monitor up front (restoring saved per-monitor tokens silently where
            // possible), so reconnects and restarts don't re-prompt.
            match build_hyprland_sessions() {
                Ok((primary, extras)) => {
                    *rdp_connection = Some(primary);
                    let mut extra = EXTRA_RDP_SESSION_INFO.lock().unwrap();
                    extra.clear();
                    extra.extend(extras);
                    // All monitors are already granted; suppress the blind
                    // extra-grant loop below.
                    TRIED_ADDITIONAL_GRANTS.store(true, Ordering::SeqCst);
                }
                Err(err) => {
                    warn!("Falling back to single Wayland portal grant: {}", err);
                    *rdp_connection = Some(new_rdp_session(request_remote_desktop(false, None)?));
                }
            }
        } else {
            *rdp_connection = Some(new_rdp_session(request_remote_desktop(false, None)?));
        }
    }

    let rdp_info = match rdp_connection.as_mut() {
        Some(res) => res,
        None => {
            return Err(Box::new(DBusError("RDP response is None.".into())));
        }
    };

    let mut capturables = capturables_from_session(rdp_info);
    append_held_extra_grants(&mut capturables);
    log_capturables("Primary Wayland portal grant", &capturables);

    if is_hyprland_session() {
        extend_with_additional_grants(&mut capturables);
        log_capturables("Final Hyprland Wayland capturables", &capturables);
        return Ok(capturables);
    }

    if capturables.len() == 1 {
        if let Some(capturable) = capturables.pop() {
            capturables = match split_workspace_capturable(capturable) {
                Ok(split) => split,
                Err(capturable) => vec![capturable],
            };
        }
    }

    // No `extend_with_additional_grants()` here: it returns immediately unless
    // `is_hyprland_session()`, and the Hyprland path already returned above, so
    // the call could never do any work.
    log_capturables("Final Wayland capturables", &capturables);

    Ok(capturables)
}

// If `is_server_running()` is true, then `screencast_portal::start` is called.
// Otherwise, `remote_desktop_portal::start` is called.
//
// If `is_server_running()` is true, `--service` process is running,
// then we can use uinput as the input method.
// Otherwise, we have to use remote_desktop_portal's input method.
//
// `screencast_portal` supports restore_token and persist_mode if the version is greater than or equal to 4.
// `remote_desktop_portal` does not support restore_token and persist_mode.
pub(crate) fn is_server_running() -> bool {
    let v = IS_SERVER_RUNNING.load(Ordering::SeqCst);
    if v > 0 {
        return v == 1;
    }

    let app_name = config::APP_NAME.read().unwrap().clone().to_lowercase();
    let output = match Command::new(CMD_SH.as_str())
        .arg("-c")
        .arg(&format!("ps aux | grep {}", app_name))
        .output()
    {
        Ok(output) => output,
        Err(_) => {
            return false;
        }
    };

    let output_str = String::from_utf8_lossy(&output.stdout);
    let is_running = output_str.contains(&format!("{} --server", app_name));
    IS_SERVER_RUNNING.store(if is_running { 1 } else { 2 }, Ordering::SeqCst);
    is_running
}

// The logical size reported by portal may be different from the size reported by `get_displays()`.
// So we need to use the workaround here.
// 1. openSUSE, KDE Plasma
// 2. Kubuntu 24.04 TLS, after running `sudo apt install plasma-workspace-wayland`
// Maybe it's a bug, and we can remove this workaround in the future.
pub fn try_fix_logical_size(shared_displays: &mut Vec<crate::Display>) {
    if !is_server_running() {
        return;
    }

    let wayland_displays = get_displays();
    if wayland_displays.displays.is_empty() {
        return;
    }

    for sd in shared_displays.iter_mut() {
        if let crate::Display::WAYLAND(d) = sd {
            let capturable = &mut d.0;
            for wd in wayland_displays.displays.iter() {
                if capturable.position.0 == wd.x && capturable.position.1 == wd.y {
                    if let Some(logical_size) = wd.logical_size {
                        if capturable.physical_size.0 != wd.width as usize
                            || capturable.physical_size.1 != wd.height as usize
                        {
                            // If "Full Workspace" is selected in the portal dialog,
                            // the physical size reported by portal may not match the display info.
                            debug!(
                            "Physical size of capturable ({:?}) does not match display info: ({:?}) - ({:?}). Skipping logical size fix.",
                            capturable.position,
                            capturable.physical_size,
                            (wd.width as usize, wd.height as usize)
                        );
                            break;
                        }

                        if capturable.logical_size.0 != logical_size.0 as usize
                            || capturable.logical_size.1 != logical_size.1 as usize
                        {
                            warn!(
                            "Fixing logical size of capturable from {:?} to {:?} based on display info {:?}.",
                            capturable.logical_size,
                            logical_size,
                            wd
                        );
                            capturable.logical_size =
                                (logical_size.0 as usize, logical_size.1 as usize);
                        }
                    }
                    break;
                }
            }
        }
    }
}

pub fn fill_displays(
    mouse_move_to: impl Fn(i32, i32),
    get_cursor_pos: fn() -> Option<(i32, i32)>,
    shared_displays: &mut Vec<crate::Display>,
) -> ResultType<()> {
    if !is_server_running() {
        return Ok(());
    }

    let mut rdp_connection = RDP_SESSION_INFO.lock().unwrap();
    let rdp_info = match rdp_connection.as_mut() {
        Some(res) => res,
        None => {
            // Unreachable
            bail!("RDP session info is None when filling display positions.");
        }
    };

    let all_displays = get_displays();
    if all_displays.displays.len() > 1 && shared_displays.len() <= 1 {
        warn!(
            "Wayland portal exposed {} capturable stream(s) for {} compositor display(s); remote monitor switching will be limited until the portal grants multiple displays.",
            shared_displays.len(),
            all_displays.displays.len()
        );
    }
    if !HAS_POSITION_ATTR.load(Ordering::SeqCst) {
        if all_displays.displays.len() > 1 {
            debug!("Multiple Wayland displays detected, adjusting stream positions accordingly.");
            try_fill_positions(
                mouse_move_to,
                get_cursor_pos,
                &all_displays,
                shared_displays,
                &mut rdp_info.streams,
            )?;
        }
        HAS_POSITION_ATTR.store(true, Ordering::SeqCst);
    }

    if all_displays.displays.len() > 1 && rdp_info.streams.len() == shared_displays.len() {
        sort_streams(&all_displays, shared_displays, &mut rdp_info.streams);
    }

    shared_displays.iter_mut().next().map(|d| {
        if let crate::Display::WAYLAND(d) = d {
            d.0.primary = true;
        }
    });

    Ok(())
}

fn try_fill_positions(
    mouse_move_to: impl Fn(i32, i32),
    get_cursor_pos: fn() -> Option<(i32, i32)>,
    displays: &Arc<Displays>,
    shared_displays: &mut Vec<crate::Display>,
    streams: &mut Vec<PwStreamInfo>,
) -> ResultType<()> {
    if is_hyprland_session() {
        config::LocalConfig::set_option(PIPEWIRE_DISPLAY_OFFSET_CONF_KEY.to_owned(), "".to_owned());
    }
    let pipewire_display_offset = config::LocalConfig::get_option(PIPEWIRE_DISPLAY_OFFSET_CONF_KEY);
    if !pipewire_display_offset.is_empty() {
        if try_fill_positions_from_cache(
            pipewire_display_offset,
            displays,
            shared_displays,
            streams,
        ) {
            return Ok(());
        }
        config::LocalConfig::set_option(PIPEWIRE_DISPLAY_OFFSET_CONF_KEY.to_owned(), "".to_owned());
    }

    let mut multi_matched_indices = Vec::new();
    for (i, sd) in shared_displays.iter_mut().enumerate() {
        if let crate::Display::WAYLAND(d) = sd {
            let capturable = &mut d.0;
            let mut match_count = 0;
            for wd in displays.displays.iter() {
                if capturable.physical_size.0 == wd.width as usize
                    && capturable.physical_size.1 == wd.height as usize
                {
                    capturable.position = (wd.x, wd.y);
                    if let Some(pw_stream) = streams.get_mut(i) {
                        pw_stream.position = (wd.x, wd.y);
                    }
                    match_count += 1;
                }
            }
            if match_count == 0 {
                warn!(
                    "No matching display found for capturable with size {:?}.",
                    capturable.physical_size
                );
            } else if match_count > 1 {
                multi_matched_indices.push(i);
            }
        }
    }

    if !multi_matched_indices.is_empty() {
        fill_multi_matched_positions(
            mouse_move_to,
            get_cursor_pos,
            displays,
            shared_displays,
            streams,
            multi_matched_indices,
        )?;
    }

    save_positions_to_cache(displays, shared_displays);
    Ok(())
}

fn try_fill_positions_from_cache(
    cache_str: String,
    displays: &Arc<Displays>,
    shared_displays: &mut Vec<crate::Display>,
    streams: &mut Vec<PwStreamInfo>,
) -> bool {
    let Ok(cache) = serde_json::from_str::<PipewireDisplayOffsetCache>(&cache_str) else {
        return false;
    };

    if cache.offsets.len() != shared_displays.len() {
        return false;
    }

    let display_key = PipewireDisplayOffsetCache::displays_to_key(displays);
    if cache.display_key != display_key {
        return false;
    }

    let restore_token = config::LocalConfig::get_option(RESTORE_TOKEN_CONF_KEY);
    if cache.restore_token != restore_token {
        return false;
    }

    for (i, sd) in shared_displays.iter_mut().enumerate() {
        if let crate::Display::WAYLAND(d) = sd {
            let capturable = &mut d.0;
            if let Some((x_off, y_off)) = cache.offsets.get(i) {
                capturable.position = (*x_off, *y_off);
                if let Some(pw_stream) = streams.get_mut(i) {
                    pw_stream.position = (*x_off, *y_off);
                }
            }
        }
    }
    true
}

fn save_positions_to_cache(displays: &Arc<Displays>, shared_displays: &Vec<crate::Display>) {
    let restore_token = config::LocalConfig::get_option(RESTORE_TOKEN_CONF_KEY);
    if restore_token.is_empty() {
        return;
    }

    let mut offsets = Vec::new();
    for sd in shared_displays.iter() {
        if let crate::Display::WAYLAND(d) = sd {
            let capturable = &d.0;
            offsets.push((capturable.position.0, capturable.position.1));
        }
    }

    let display_key = PipewireDisplayOffsetCache::displays_to_key(displays);
    let cache = PipewireDisplayOffsetCache {
        display_key,
        restore_token,
        offsets,
    };

    if let Ok(s) = serde_json::to_string(&cache) {
        config::LocalConfig::set_option(PIPEWIRE_DISPLAY_OFFSET_CONF_KEY.to_owned(), s);
    }
}

fn compare_left_up_corner(w: usize, d1: &[u8], d2: &[u8]) -> bool {
    if w == 0 {
        return false;
    }
    if d1.len() != d2.len() {
        return false;
    }
    let bpp = 4; // BGR0/RGB0
    let stride = w.saturating_mul(bpp);
    if stride == 0 || d1.len() < stride || d2.len() < stride {
        return false;
    }
    let h = d1.len() / stride;
    if h == 0 {
        return false;
    }

    let roi_w = std::cmp::min(36, w);
    let roi_h = std::cmp::min(36, h);
    let mut diff_px = 0usize;
    let total_px = roi_w * roi_h;
    // Minimum number of differing pixels required to consider images different.
    const MIN_DIFF_PIXELS: usize = 8;
    // Divisor for threshold calculation: allows up to 1/8 of ROI pixels to differ before returning true.
    const DIFF_THRESHOLD_DIVISOR: usize = 8;
    let threshold = std::cmp::max(MIN_DIFF_PIXELS, total_px / DIFF_THRESHOLD_DIVISOR);

    for y in 0..roi_h {
        let row_off = y * stride;
        for x in 0..roi_w {
            let i = row_off + x * bpp;
            let a = &d1[i..i + bpp];
            let b = &d2[i..i + bpp];
            if a != b {
                diff_px += 1;
                if diff_px >= threshold {
                    return true;
                }
            }
        }
    }
    false
}

fn fill_multi_matched_positions(
    mouse_move_to: impl Fn(i32, i32),
    get_cursor_pos: fn() -> Option<(i32, i32)>,
    displays: &Arc<Displays>,
    shared_displays: &mut Vec<crate::Display>,
    streams: &mut Vec<PwStreamInfo>,
    multi_matched_indices: Vec<usize>,
) -> ResultType<()> {
    debug!(
        "Multiple capturables ({:?}) match the same display size, attempting to disambiguate positions.",
    &multi_matched_indices);
    if multi_matched_indices.is_empty() {
        return Ok(());
    }

    let is_support_embeded_cursor = get_available_cursor_modes()
        .ok()
        .map(|modes| modes & 0x2 != 0)
        .unwrap_or(false);
    if is_support_embeded_cursor {
        fill_multi_matched_positions_cursor(
            mouse_move_to,
            get_cursor_pos,
            displays,
            shared_displays,
            streams,
            multi_matched_indices,
        )?;
    }

    Ok(())
}

fn mouse_move_to_(
    mouse_move_to: &impl Fn(i32, i32),
    get_cursor_pos: fn() -> Option<(i32, i32)>,
    x: i32,
    y: i32,
) {
    const MOVE_MOUSE_TIMEOUT: Duration = Duration::from_millis(150);
    let start = std::time::Instant::now();
    while start.elapsed() < MOVE_MOUSE_TIMEOUT {
        mouse_move_to(x, y);
        std::thread::sleep(Duration::from_millis(20));
        if let Some((x1, y1)) = get_cursor_pos() {
            if x1 == x && y1 == y {
                return;
            }
        }
    }
    warn!(
        "Failed to move mouse to ({}, {}) within timeout: {:?}.",
        x, y, &MOVE_MOUSE_TIMEOUT
    );
}

fn fill_multi_matched_positions_cursor(
    mouse_move_to: impl Fn(i32, i32),
    get_cursor_pos: fn() -> Option<(i32, i32)>,
    displays: &Arc<Displays>,
    shared_displays: &mut Vec<crate::Display>,
    streams: &mut Vec<PwStreamInfo>,
    multi_matched_indices: Vec<usize>,
) -> ResultType<()> {
    // This creates a new remote desktop session for cursor-based position detection.
    // The session is temporary, used only for disambiguation, and is dropped after detection completes.
    let (conn, fd, streams_with_cursor, _session, _is_support_restore_token) =
        request_remote_desktop(true, None)?;
    let conn = Arc::new(conn);

    let mut matched_indices = Vec::new();
    const CAPTURE_TIMEOUT_MS: u64 = 1_000;
    for idx in multi_matched_indices {
        match (
            shared_displays.get_mut(idx),
            streams.get_mut(idx),
            streams_with_cursor.get(idx),
        ) {
            (Some(crate::Display::WAYLAND(d)), Some(pw_stream), Some(pw_stream_with_cursor)) => {
                // Check if only one display matches the size
                let mut match_count = 0;
                for (i, wd) in displays.displays.iter().enumerate() {
                    if matched_indices.contains(&i) {
                        continue;
                    }
                    if d.0.physical_size.0 == wd.width as usize
                        && d.0.physical_size.1 == wd.height as usize
                    {
                        match_count += 1;
                    }
                }
                if match_count == 0 {
                    error!(
                        "No matching display found for capturable with size {:?}.",
                        d.0.physical_size
                    );
                    continue;
                }
                if match_count == 1 {
                    for (i, wd) in displays.displays.iter().enumerate() {
                        if matched_indices.contains(&i) {
                            continue;
                        }
                        if d.0.physical_size.0 == wd.width as usize
                            && d.0.physical_size.1 == wd.height as usize
                        {
                            d.0.position = (wd.x, wd.y);
                            pw_stream.position = (wd.x, wd.y);
                            matched_indices.push(i);
                            debug!(
                                "Disambiguated position for capturable with size {:?} to ({}, {}).",
                                d.0.physical_size, wd.x, wd.y
                            );
                            break;
                        }
                    }
                    continue;
                }

                // Move the mouse to a neutral position first,
                // to avoid interference from previous position.
                mouse_move_to_(&mouse_move_to, get_cursor_pos, 300, 300);

                let mut rec = PipeWireRecorder::new(PipeWireCapturable {
                    dbus_conn: conn.clone(),
                    fd: fd.clone(),
                    path: pw_stream_with_cursor.path,
                    source_type: pw_stream_with_cursor.source_type,
                    crop: None,
                    primary: false,
                    position: pw_stream_with_cursor.position,
                    logical_size: pw_stream_with_cursor.size,
                    physical_size: (0, 0),
                })?;
                // Take first frame and copy owned buffer to avoid borrow across second capture
                let (is_bgr, w, first_buf): (bool, usize, Vec<u8>) =
                    match rec.capture(CAPTURE_TIMEOUT_MS) {
                        Ok(PixelProvider::BGR0(w, _, data1)) => (true, w, data1.to_vec()),
                        Ok(PixelProvider::RGB0(w, _, data1)) => (false, w, data1.to_vec()),
                        Ok(_) => {
                            error!("Unexpected pixel format on first capture.");
                            continue;
                        }
                        Err(e) => {
                            error!(
                                "Failed to capture screen for position disambiguation: {}",
                                e
                            );
                            continue;
                        }
                    };

                let matched_len = matched_indices.len();
                for (i, wd) in displays.displays.iter().enumerate() {
                    if matched_indices.contains(&i) {
                        continue;
                    }

                    if wd.width as usize == d.0.physical_size.0
                        && wd.height as usize == d.0.physical_size.1
                    {
                        mouse_move_to_(&mouse_move_to, get_cursor_pos, wd.x + 8, wd.y + 8);
                        rec.saved_raw_data.clear();
                        match rec.capture(CAPTURE_TIMEOUT_MS) {
                            Ok(PixelProvider::BGR0(_, _, data2)) if is_bgr => {
                                if compare_left_up_corner(w, &first_buf, data2) {
                                    d.0.position = (wd.x, wd.y);
                                    pw_stream.position = (wd.x, wd.y);
                                    matched_indices.push(i);
                                    debug!(
                                        "Disambiguated position for capturable with size {:?} to ({}, {}).",
                                        d.0.physical_size, wd.x, wd.y
                                    );
                                    break;
                                }
                            }
                            Ok(PixelProvider::RGB0(_, _, data2)) if !is_bgr => {
                                if compare_left_up_corner(w, &first_buf, data2) {
                                    d.0.position = (wd.x, wd.y);
                                    pw_stream.position = (wd.x, wd.y);
                                    matched_indices.push(i);
                                    debug!(
                                        "Disambiguated position for capturable with size {:?} to ({}, {}).",
                                        d.0.physical_size, wd.x, wd.y
                                    );
                                    break;
                                }
                            }
                            Ok(_) => {
                                // unreachable
                                error!("Pixel format changed between captures, cannot disambiguate position.");
                            }
                            Err(e) => {
                                error!(
                                    "Failed to capture screen for position disambiguation: {}",
                                    e
                                );
                            }
                        }
                    }
                }
                if matched_len == matched_indices.len() {
                    error!(
                        "Failed to disambiguate position for capturable with size {:?}.",
                        d.0.physical_size
                    );
                }
            }
            _ => {}
        }
    }

    Ok(())
}

fn sort_streams(
    displays: &Arc<Displays>,
    shared_displays: &mut Vec<crate::Display>,
    streams: &mut Vec<PwStreamInfo>,
) {
    if streams.is_empty() {
        // unreachable
        error!("No streams available to sort.");
        return;
    }

    // put the main display first, then the rest by the order of displays
    let mut display_order: Vec<(i32, i32)> = Vec::new();
    if let Some(d) = displays.displays.get(displays.primary) {
        display_order.push((d.x, d.y));
    }
    for (i, d) in displays.displays.iter().enumerate() {
        if i != displays.primary {
            display_order.push((d.x, d.y));
        }
    }

    let original_stream_count = streams.len();
    let original_display_count = shared_displays.len();
    let mut sorted_streams = Vec::new();
    let mut sorted_shared_displays = Vec::new();
    // Move matching items in order without cloning
    for (x, y) in display_order.into_iter() {
        for i in 0..streams.len() {
            if streams[i].position.0 == x && streams[i].position.1 == y {
                sorted_streams.push(streams.remove(i));
                // shared_displays.len() must be equal to streams.len()
                // But we still check the length to avoid panic
                if shared_displays.len() > i {
                    sorted_shared_displays.push(shared_displays.remove(i));
                }
                break;
            }
        }
    }
    if sorted_streams.is_empty()
        || sorted_streams.len() != original_stream_count
        || sorted_shared_displays.len() != original_display_count
    {
        debug!(
            "Skipping stream sort due to partial position match: sorted_streams={}, streams={}, sorted_displays={}, displays={}",
            sorted_streams.len(),
            original_stream_count,
            sorted_shared_displays.len(),
            original_display_count
        );
        return;
    }
    *streams = sorted_streams;
    *shared_displays = sorted_shared_displays;
}
