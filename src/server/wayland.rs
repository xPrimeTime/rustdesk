use super::*;
use hbb_common::{allow_err, platform::linux::DISTRO};
use scrap::{
    is_cursor_embedded, set_map_err,
    wayland::pipewire::{fill_displays, is_hyprland_session, try_fix_logical_size},
    Capturer, Display,
};
use std::collections::HashMap;
use std::io;

use crate::{
    client::{
        SCRAP_OTHER_VERSION_OR_X11_REQUIRED, SCRAP_UBUNTU_HIGHER_REQUIRED, SCRAP_X11_REQUIRED,
    },
    platform::linux::is_x11,
};

lazy_static::lazy_static! {
    static ref CAP_DISPLAY_INFO: RwLock<HashMap<usize, u64>> = RwLock::new(HashMap::new());
    static ref PIPEWIRE_INITIALIZED: RwLock<bool> = RwLock::new(false);
    static ref LOG_SCRAP_COUNT: Mutex<u32> = Mutex::new(0);
    static ref ACTIVE_DISPLAY_COUNT: RwLock<usize> = RwLock::new(0);
}

pub fn init() {
    set_map_err(map_err_scrap);
}

pub(super) fn increment_active_display_count() -> usize {
    let mut count = ACTIVE_DISPLAY_COUNT.write().unwrap();
    *count += 1;
    *count
}

pub(super) fn decrement_active_display_count() -> usize {
    let mut count = ACTIVE_DISPLAY_COUNT.write().unwrap();
    if *count > 0 {
        *count -= 1;
    }
    *count
}

pub(super) fn active_display_count() -> usize {
    *ACTIVE_DISPLAY_COUNT.read().unwrap()
}

fn map_err_scrap(err: String) -> io::Error {
    // to-do: Handle error better, do not restart server
    if err.starts_with("Did not receive a reply") {
        log::error!("Fatal pipewire error, {}", &err);
        std::process::exit(-1);
    }

    if DISTRO.name.to_uppercase() == "Ubuntu".to_uppercase() {
        if DISTRO.version_id < "21".to_owned() {
            io::Error::new(io::ErrorKind::Other, SCRAP_UBUNTU_HIGHER_REQUIRED)
        } else {
            try_log(&err);
            io::Error::new(io::ErrorKind::Other, err)
        }
    } else {
        try_log(&err);
        if err.contains("org.freedesktop.portal")
            || err.contains("pipewire")
            || err.contains("dbus")
        {
            io::Error::new(io::ErrorKind::Other, SCRAP_OTHER_VERSION_OR_X11_REQUIRED)
        } else {
            io::Error::new(io::ErrorKind::Other, SCRAP_X11_REQUIRED)
        }
    }
}

fn try_log(err: &String) {
    let mut lock_count = LOG_SCRAP_COUNT.lock().unwrap();
    if *lock_count >= 1000000 {
        return;
    }
    if *lock_count % 10000 == 0 {
        log::error!("Failed scrap {}", err);
    }
    *lock_count += 1;
}

struct CapDisplayInfo {
    rects: Vec<((i32, i32), usize, usize)>,
    displays: Vec<DisplayInfo>,
    num: usize,
    primary: usize,
    current: usize,
    display: Display,
}

fn align_display_infos_to_rects(
    rects: &[((i32, i32), usize, usize)],
    displays: &[DisplayInfo],
) -> Vec<DisplayInfo> {
    if rects.is_empty() || displays.is_empty() {
        return displays.to_vec();
    }

    let mut remaining = displays.to_vec();
    let mut aligned = Vec::with_capacity(rects.len());

    for (origin, width, height) in rects {
        if let Some(idx) = remaining.iter().position(|display| {
            display.x == origin.0
                && display.y == origin.1
                && display.width == *width as i32
                && display.height == *height as i32
        }) {
            aligned.push(remaining.remove(idx));
        }
    }

    if aligned.len() == rects.len() {
        aligned
    } else {
        displays.to_vec()
    }
}

fn get_desktop_rect_from_displays(displays: &[Display]) -> Option<(i32, i32, i32, i32)> {
    if displays.is_empty() {
        return None;
    }

    if displays.len() == 1 {
        let d = &displays[0];
        let logical_w = d.logical_width() as i32;
        let logical_h = d.logical_height() as i32;
        let width = if logical_w > 0 {
            logical_w
        } else {
            d.width() as i32
        };
        let height = if logical_h > 0 {
            logical_h
        } else {
            d.height() as i32
        };
        return Some((
            d.origin().0,
            d.origin().0 + width,
            d.origin().1,
            d.origin().1 + height,
        ));
    }

    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_x = i32::MIN;
    let mut max_y = i32::MIN;

    for d in displays {
        min_x = min_x.min(d.origin().0);
        min_y = min_y.min(d.origin().1);

        let logical_w = d.logical_width() as i32;
        let logical_h = d.logical_height() as i32;
        let width = if logical_w > 0 {
            logical_w
        } else {
            d.width() as i32
        };
        let height = if logical_h > 0 {
            logical_h
        } else {
            d.height() as i32
        };

        max_x = max_x.max(d.origin().0 + width);
        max_y = max_y.max(d.origin().1 + height);
    }

    Some((min_x, max_x, min_y, max_y))
}

fn get_mouse_resolution_rect(displays: &[Display]) -> Option<(i32, i32, i32, i32)> {
    let compositor_rect = scrap::wayland::display::get_desktop_rect_for_uinput();
    let capturable_rect = get_desktop_rect_from_displays(displays);
    if is_hyprland_session() && compositor_rect.is_some() {
        // Hyprland portal streams are per-monitor capture frames and may all
        // report origin (0, 0). Uinput needs compositor desktop coordinates.
        return compositor_rect;
    }
    compositor_rect.or(capturable_rect)
}

#[tokio::main(flavor = "current_thread")]
pub(super) async fn ensure_inited() -> ResultType<()> {
    check_init().await
}

pub(super) fn is_inited() -> Option<Message> {
    if is_x11() {
        None
    } else {
        if CAP_DISPLAY_INFO.read().unwrap().is_empty() {
            let mut msg_out = Message::new();
            let res = MessageBox {
                msgtype: "nook-nocancel-hasclose".to_owned(),
                title: "Wayland".to_owned(),
                text: "Please Select the screen to be shared(Operate on the peer side).".to_owned(),
                link: "".to_owned(),
                ..Default::default()
            };
            msg_out.set_message_box(res);
            Some(msg_out)
        } else {
            None
        }
    }
}

pub(super) async fn check_init() -> ResultType<()> {
    if !is_x11() {
        if CAP_DISPLAY_INFO.read().unwrap().is_empty() {
            let mut pending_mouse_resolution = None;
            if crate::input_service::wayland_use_uinput() {
                if let Some((minx, maxx, miny, maxy)) =
                    scrap::wayland::display::get_desktop_rect_for_uinput()
                {
                    log::info!(
                        "update mouse resolution: ({}, {}), ({}, {})",
                        minx,
                        maxx,
                        miny,
                        maxy
                    );
                    allow_err!(
                        input_service::update_mouse_resolution(minx, maxx, miny, maxy).await
                    );
                } else {
                    log::warn!("Failed to get desktop rect for uinput");
                }
            }

            {
                let mut lock = CAP_DISPLAY_INFO.write().unwrap();
                if lock.is_empty() {
                    // Check if PipeWire is already initialized to prevent duplicate recorder creation
                    if *PIPEWIRE_INITIALIZED.read().unwrap() {
                        log::warn!(
                            "wayland_diag: PipeWire marked initialized but no capturers are registered; resetting state"
                        );
                        *PIPEWIRE_INITIALIZED.write().unwrap() = false;
                    }

                    let mut all = Display::all()?;
                    if all.is_empty() {
                        bail!("No Wayland capturable displays available yet");
                    }
                    log::debug!("Initializing displays with fill_displays()");
                    {
                        let temp_mouse_move_handle = input_service::TemporaryMouseMoveHandle::new();
                        let move_mouse_to = |x, y| temp_mouse_move_handle.move_mouse_to(x, y);
                        fill_displays(move_mouse_to, crate::get_cursor_pos, &mut all)?;
                    }
                    log::debug!("Attempting to fix logical size with try_fix_logical_size()");
                    try_fix_logical_size(&mut all);
                    if all.is_empty() {
                        bail!("Wayland capturable displays disappeared during initialization");
                    }
                    if crate::input_service::wayland_use_uinput() {
                        if let Some((minx, maxx, miny, maxy)) = get_mouse_resolution_rect(&all) {
                            log::info!(
                                "update mouse resolution from Wayland displays: ({}, {}), ({}, {})",
                                minx,
                                maxx,
                                miny,
                                maxy
                            );
                            pending_mouse_resolution = Some((minx, maxx, miny, maxy));
                        }
                    }
                    *PIPEWIRE_INITIALIZED.write().unwrap() = true;
                    let num = all.len();
                    let primary = super::display_service::get_primary_2(&all);
                    super::display_service::check_update_displays(&all);
                    let mut displays = super::display_service::get_sync_displays();
                    for display in displays.iter_mut() {
                        display.cursor_embedded = is_cursor_embedded();
                    }

                    let mut rects: Vec<((i32, i32), usize, usize)> = Vec::new();
                    for d in &all {
                        rects.push((d.origin(), d.width(), d.height()));
                    }
                    let displays = align_display_infos_to_rects(&rects, &displays);

                    log::debug!(
                        "#displays={}, primary={}, rects: {:?}, cpus={}/{}",
                        num,
                        primary,
                        rects,
                        num_cpus::get_physical(),
                        num_cpus::get()
                    );

                    // Cache display metadata only. The PipeWire capturer is opened
                    // on demand for the selected monitor to avoid multiple active
                    // GStreamer pipelines during Wayland display switching.
                    for (idx, display) in all.into_iter().enumerate() {
                        let cap_display_info = Box::into_raw(Box::new(CapDisplayInfo {
                            rects: rects.clone(),
                            displays: displays.clone(),
                            num,
                            primary,
                            current: idx,
                            display,
                        }));

                        lock.insert(idx, cap_display_info as u64);
                    }

                    if lock.is_empty() {
                        *PIPEWIRE_INITIALIZED.write().unwrap() = false;
                        bail!("No Wayland capturers were created");
                    }
                }
            }
            if let Some((minx, maxx, miny, maxy)) = pending_mouse_resolution {
                allow_err!(input_service::update_mouse_resolution(minx, maxx, miny, maxy).await);
            }
        }
    }
    Ok(())
}

pub(super) async fn get_displays() -> ResultType<Vec<DisplayInfo>> {
    check_init().await?;
    let cap_map = CAP_DISPLAY_INFO.read().unwrap();
    if let Some(addr) = cap_map.values().next() {
        let cap_display_info: *const CapDisplayInfo = *addr as _;
        unsafe {
            let cap_display_info = &*cap_display_info;
            Ok(cap_display_info.displays.clone())
        }
    } else {
        bail!("Failed to get capturer display info");
    }
}

pub(super) fn get_primary() -> ResultType<usize> {
    let cap_map = CAP_DISPLAY_INFO.read().unwrap();
    if let Some(addr) = cap_map.values().next() {
        let cap_display_info: *const CapDisplayInfo = *addr as _;
        unsafe {
            let cap_display_info = &*cap_display_info;
            Ok(cap_display_info.primary)
        }
    } else {
        bail!("Failed to get capturer display info");
    }
}

pub fn clear() {
    if is_x11() {
        return;
    }
    let mut write_lock = CAP_DISPLAY_INFO.write().unwrap();
    for (_, addr) in write_lock.iter() {
        let cap_display_info: *mut CapDisplayInfo = *addr as _;
        unsafe {
            let _box_cap_display_info = Box::from_raw(cap_display_info);
        }
    }
    write_lock.clear();

    // Reset PipeWire initialization flag to allow recreation on next init
    *PIPEWIRE_INITIALIZED.write().unwrap() = false;
}

pub(super) fn get_capturer_for_display(
    display_idx: usize,
) -> ResultType<super::video_service::CapturerInfo> {
    if is_x11() {
        bail!("Do not call this function if not wayland");
    }
    let cap_map = CAP_DISPLAY_INFO.read().unwrap();
    if let Some(addr) = cap_map.get(&display_idx) {
        let cap_display_info: *const CapDisplayInfo = *addr as _;
        unsafe {
            let cap_display_info = &*cap_display_info;
            let rect = cap_display_info.rects[cap_display_info.current];
            let capturer = Capturer::new(cap_display_info.display.clone()).with_context(|| {
                format!("Failed to create capturer for display {}", display_idx)
            })?;
            Ok(super::video_service::CapturerInfo {
                origin: rect.0,
                width: rect.1,
                height: rect.2,
                ndisplay: cap_display_info.num,
                current: cap_display_info.current,
                privacy_mode_id: 0,
                _capturer_privacy_mode_id: 0,
                capturer: Box::new(capturer),
            })
        }
    } else {
        bail!(
            "Failed to get capturer display info for display {}",
            display_idx
        );
    }
}

pub fn common_get_error() -> String {
    if DISTRO.name.to_uppercase() == "Ubuntu".to_uppercase() {
        if DISTRO.version_id < "21".to_owned() {
            return "".to_owned();
        }
    } else {
        // to-do: check other distros
    }
    return "".to_owned();
}
