//! The calibration window: guides the user through focusing a camera's
//! lenses, fitting them, saying how its cameras are mounted, aligning them,
//! and choosing the view and blend. Each change takes effect as soon as it
//! is saved.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use calibration::Check;
use calibration::chessboard::{self, Board, COVERAGE_AREAS, Corners, GOOD_ERROR};
use dioxus::desktop::{Config, LogicalSize, WeakDesktopContext, WindowBuilder, icon_from_memory};
use dioxus::prelude::*;
use protocol::{Side, rig};
use stitching::geometry::{AspectRatio, Blend, Orientation, View};
use tokio::runtime::Handle;

use super::controls::NumberSlider;
use super::{BACKGROUND, ICON, STYLE, use_channel, use_hub, use_runtime, use_watch};
use crate::calibrator::{
    self, ALIGNMENT_PAIRS, Aligner, AlignmentState, LensCalibration, LensCommand, ShownAlignment,
    Sighting,
};
use crate::hub::{AutofocusState, CameraLink, Hub};
use crate::server::Feed;

thread_local! {
    /// The open calibration windows, by camera.
    static WINDOWS: RefCell<HashMap<String, WeakDesktopContext>> = RefCell::default();
}

/// Opens the calibration window for `camera`, or brings it to the front.
pub fn open(hub: Arc<Hub>, runtime: Handle, camera: String) {
    let open = WINDOWS.with_borrow(|windows| windows.get(&camera).and_then(|window| window.upgrade()));
    if let Some(window) = open {
        window.window.set_focus();
        return;
    }
    let dom = VirtualDom::new_with_props(
        CalibrationWindow,
        CalibrationWindowProps {
            camera: camera.clone(),
        },
    )
    .with_root_context(hub)
    .with_root_context(runtime);
    let window = WindowBuilder::new()
        .with_title(format!("Calibrate {camera}"))
        .with_inner_size(LogicalSize::new(1200.0, 880.0));
    let config = Config::new()
        .with_window(window)
        .with_icon(icon_from_memory(ICON).expect("valid icon"))
        .with_menu(None)
        .with_background_color(BACKGROUND);
    let pending = dioxus::desktop::window().new_window(dom, config);
    spawn(async move {
        let window = pending.await;
        WINDOWS.with_borrow_mut(|windows| windows.insert(camera, Rc::downgrade(&window)));
    });
}

/// Closes every calibration window.
pub fn close_all() {
    WINDOWS.with_borrow_mut(|windows| {
        for (_, window) in windows.drain() {
            if let Some(window) = window.upgrade() {
                window.close();
            }
        }
    });
}

#[derive(Clone, Copy, PartialEq)]
enum Step {
    Focus,
    Lenses,
    Mount,
    Align,
    ViewAndBlend,
}

impl Step {
    const ALL: [Step; 5] = [Step::Focus, Step::Lenses, Step::Mount, Step::Align, Step::ViewAndBlend];

    fn title(self) -> &'static str {
        match self {
            Step::Focus => "1. Focus",
            Step::Lenses => "2. Lenses",
            Step::Mount => "3. Mount",
            Step::Align => "4. Align",
            Step::ViewAndBlend => "5. View & blend",
        }
    }
}

/// What the window works on, for as long as it is open.
struct Sessions {
    link: Arc<CameraLink>,
    lenses: [LensCalibration; 2],
    aligner: Aligner,
    _calibrating: Calibrating,
}

/// Marks a camera as being calibrated while it lives, so its panorama is
/// stitched here to be previewed.
struct Calibrating {
    hub: Arc<Hub>,
    camera: String,
}

impl Calibrating {
    fn new(hub: Arc<Hub>, camera: String) -> Calibrating {
        hub.calibrating.send_modify(|cameras| {
            cameras.insert(camera.clone());
        });
        Calibrating { hub, camera }
    }
}

impl Drop for Calibrating {
    fn drop(&mut self) {
        self.hub.calibrating.send_modify(|cameras| {
            cameras.remove(&self.camera);
        });
    }
}

#[component]
fn CalibrationWindow(camera: String) -> Element {
    let hub = use_hub();
    let runtime = use_runtime();
    let sessions = use_hook(|| {
        hub.link(&camera).map(|link| {
            let directory = hub.paths.calibration(&camera);
            Rc::new(Sessions {
                lenses: Side::BOTH.map(|side| LensCalibration::start(&runtime, link.clone(), side, &directory)),
                aligner: Aligner::new(&runtime, &directory),
                _calibrating: Calibrating::new(hub.clone(), camera.clone()),
                link,
            })
        })
    });
    let mut step = use_signal(|| Step::Focus);
    let Some(sessions) = sessions else {
        return rsx! {
            style { {STYLE} }
            p { class: "empty", "{camera} hasn't connected yet." }
        };
    };
    use_context_provider(|| sessions);
    rsx! {
        style { {STYLE} }
        div { class: "calibration",
            nav { class: "steps",
                for each in Step::ALL {
                    button {
                        class: if step() == each { "active" },
                        onclick: move |_| step.set(each),
                        "{each.title()}"
                    }
                }
            }
            div { class: "step",
                match step() {
                    Step::Focus => rsx! { FocusStep { camera } },
                    Step::Lenses => rsx! { LensStep { camera } },
                    Step::Mount => rsx! { MountStep { camera } },
                    Step::Align => rsx! { AlignStep { camera } },
                    Step::ViewAndBlend => rsx! { ViewStep { camera } },
                }
            }
        }
    }
}

fn use_sessions() -> Rc<Sessions> {
    use_context()
}

fn side_name(side: Side) -> &'static str {
    match side {
        Side::Left => "Left",
        Side::Right => "Right",
    }
}

fn camera_feed(side: Side) -> Feed {
    match side {
        Side::Left => Feed::Left,
        Side::Right => Feed::Right,
    }
}

/// The calibration `camera` is stitched with now, and how it is mounted.
fn current_mount(hub: &Hub, camera: &str, link: &CameraLink) -> [Orientation; 2] {
    hub.settings
        .borrow()
        .cameras
        .get(camera)
        .and_then(|camera| camera.setup.mount)
        .unwrap_or_else(|| {
            link.reported_calibration
                .borrow()
                .cameras
                .each_ref()
                .map(|camera| camera.orientation)
        })
}

#[component]
fn Checks(checks: Vec<Check>) -> Element {
    rsx! {
        ul { class: "checks",
            for check in checks {
                li { class: if check.passed { "passed" } else { "failed" },
                    span { class: "mark", if check.passed { "✓" } else { "✗" } }
                    "{check.description}"
                }
            }
        }
    }
}

#[component]
fn FocusStep(camera: String) -> Element {
    let hub = use_hub();
    let sessions = use_sessions();
    let autofocus = use_channel(&sessions.link.autofocus);
    let settings = use_watch(|hub| &hub.settings);
    let setup = settings.read().cameras.get(&camera).map(|camera| camera.setup.clone()).unwrap_or_default();
    let lens_positions = setup.lens_positions.map(|positions| setup.as_shown(positions));
    let state = *autofocus.read();
    rsx! {
        p {
            "Point the rig at what it will record, then focus. Each camera focuses on the middle of its view, \
             and its lens is held there. Focusing closer makes things look a little bigger, which the stitching \
             makes up for."
        }
        div { class: "toolbar",
            button {
                disabled: matches!(state, AutofocusState::Requested(_)),
                onclick: {
                    let (hub, camera) = (hub.clone(), camera.clone());
                    move |_| hub.autofocus(&camera)
                },
                "Focus"
            }
        }
        match state {
            AutofocusState::Idle => rsx! {},
            AutofocusState::Requested(_) => rsx! { p { "Focusing… A camera waits until it isn't recording." } },
            AutofocusState::Failed(outcome) => {
                let focused = setup.as_shown(outcome.lens_positions.map(|position| position.is_some()));
                let message = match focused {
                    [false, true] => "The left camera found nothing to focus on.",
                    [true, false] => "The right camera found nothing to focus on.",
                    _ => "Neither camera found anything to focus on.",
                };
                rsx! { p { class: "error", "{message} Point the rig at something with more detail and try again." } }
            }
        }
        table { class: "values",
            for side in Side::BOTH {
                tr {
                    td { "{side_name(side)} lens" }
                    td {
                        match lens_positions {
                            Some(positions) => format!("{:.2}", positions[side.index()]),
                            None => "not focused yet".to_owned(),
                        }
                    }
                }
            }
        }
        p { class: "muted", "Lens positions are libcamera's: the higher, the closer a lens focuses." }
        div { class: "figure-pair",
            for side in Side::BOTH {
                figure {
                    img { src: camera_feed(side).url(&camera) }
                    figcaption { "{side_name(side)} camera" }
                }
            }
        }
    }
}

#[component]
fn LensStep(camera: String) -> Element {
    let sessions = use_sessions();
    let mut side = use_signal(|| Side::Left);
    use_effect({
        let sessions = sessions.clone();
        move || {
            let shown = side();
            for each in Side::BOTH {
                sessions.lenses[each.index()].command(LensCommand::Watch(each == shown));
            }
        }
    });
    use_drop({
        let sessions = sessions.clone();
        move || {
            for lens in &sessions.lenses {
                lens.command(LensCommand::Watch(false));
            }
        }
    });
    rsx! {
        div { class: "toolbar",
            for each in Side::BOTH {
                button {
                    class: if side() == each { "active" },
                    onclick: move |_| side.set(each),
                    "{side_name(each)} camera"
                }
            }
        }
        {rsx! { LensSessionView { key: "{side().index()}", camera, side: side() } }}
    }
}

/// Points of an SVG polygon around the board.
fn outline(board: Board, corners: &Corners) -> String {
    let (columns, rows) = (board.columns as usize, board.rows as usize);
    [0, columns - 1, columns * rows - 1, columns * (rows - 1)]
        .iter()
        .filter_map(|&index| corners.get(index))
        .map(|corner| format!("{:.1},{:.1}", corner.x, corner.y))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Points of an SVG polyline through the board's corners, row by row.
fn rows_path(corners: &Corners) -> String {
    corners
        .iter()
        .map(|corner| format!("{:.1},{:.1}", corner.x, corner.y))
        .collect::<Vec<_>>()
        .join(" ")
}

#[component]
fn LensSessionView(camera: String, side: Side) -> Element {
    let hub = use_hub();
    let sessions = use_sessions();
    let session = use_channel(&sessions.lenses[side.index()].state);
    let session = session.read();
    let (width, height) = calibrator::SIGHTING_SIZE;
    let board = session.board;
    let sightings: Vec<Corners> = session.pictures.iter().map(|p| p.corners.clone()).collect();
    let coverage = chessboard::coverage(&sightings, calibrator::SIGHTING_SIZE);
    let (message, sighting_class) = match &session.sighting {
        Sighting::NoBoard => ("Hold the chessboard up in front of the camera.", "none"),
        Sighting::Moving(_) => ("Hold still…", "moving"),
        Sighting::Ready(_) => ("Ready: take a picture.", "ready"),
        Sighting::AlreadyTaken(_) => ("There's a picture of the board here already. Move it somewhere new.", "taken"),
        Sighting::JustTaken(_) => ("Picture taken.", "just-taken"),
    };
    let set_board = |change: fn(&mut Board, u32)| {
        let commands = sessions.clone();
        move |e: FormEvent| {
            if let Ok(value) = e.value().parse::<u32>()
                && (3..=30).contains(&value)
            {
                let mut changed = board;
                change(&mut changed, value);
                for lens in &commands.lenses {
                    lens.command(LensCommand::SetBoard(changed));
                }
            }
        }
    };
    let errors = session.fit.as_ref().map(|fit| fit.picture_errors.clone());
    let area_width = width as f64 / COVERAGE_AREAS as f64;
    let area_height = height as f64 / COVERAGE_AREAS as f64;
    rsx! {
        div { class: "lens-step",
            div { class: "lens-main",
                div { class: "live",
                    img { src: camera_feed(side).url(&camera) }
                    svg {
                        view_box: "0 0 {width} {height}",
                        preserve_aspect_ratio: "xMidYMid meet",
                        for (row, areas) in coverage.iter().enumerate() {
                            for (column, covered) in areas.iter().enumerate() {
                                rect {
                                    class: if *covered { "area covered" } else { "area" },
                                    x: "{column as f64 * area_width}",
                                    y: "{row as f64 * area_height}",
                                    width: "{area_width}",
                                    height: "{area_height}",
                                }
                            }
                        }
                        for picture in session.pictures.iter() {
                            polygon { class: "taken", points: outline(board, &picture.corners) }
                        }
                        if let Some(corners) = session.sighting.corners() {
                            g { class: "sighting {sighting_class}",
                                polygon { points: outline(board, corners) }
                                polyline { points: rows_path(corners) }
                                for corner in corners.iter() {
                                    circle { cx: "{corner.x}", cy: "{corner.y}", r: "4" }
                                }
                            }
                        }
                    }
                    div { class: "live-message {sighting_class}", "{message}" }
                }
                div { class: "row board",
                    span { "Inner corners" }
                    input {
                        r#type: "number",
                        min: 3,
                        max: 30,
                        value: "{board.columns}",
                        onchange: set_board(|board, value| board.columns = value),
                    }
                    span { "across," }
                    input {
                        r#type: "number",
                        min: 3,
                        max: 30,
                        value: "{board.rows}",
                        onchange: set_board(|board, value| board.rows = value),
                    }
                    span { "down" }
                    label { class: "check",
                        input {
                            r#type: "checkbox",
                            checked: session.automatic,
                            onchange: {
                                let sessions = sessions.clone();
                                move |e: FormEvent| {
                                    sessions.lenses[side.index()].command(LensCommand::SetAutomatic(e.checked()))
                                }
                            },
                        }
                        "Take pictures automatically"
                    }
                    button {
                        disabled: session.sighting.corners().is_none(),
                        onclick: {
                            let sessions = sessions.clone();
                            move |_| sessions.lenses[side.index()].command(LensCommand::TakePicture)
                        },
                        "Take picture"
                    }
                    button {
                        disabled: session.pictures.is_empty(),
                        onclick: {
                            let sessions = sessions.clone();
                            move |_| sessions.lenses[side.index()].command(LensCommand::Clear)
                        },
                        "Start over"
                    }
                }
                p { class: "muted",
                    "Print a chessboard (9 by 7 squares has 8 by 6 inner corners) with a white margin around it, stick it to \
                     something flat, and hold it up \
                     in front of the camera in many places: near every edge and corner, nearer and farther, turned and \
                     tilted different ways. A picture is taken each time it's held still somewhere new. The shaded areas \
                     are where it still needs to go."
                }
                div { class: "pictures",
                    for (index, picture) in session.pictures.iter().enumerate() {
                        {
                            let error = errors.as_ref().and_then(|errors| errors.get(index)).copied();
                            let poor = error.is_some_and(|error| error > 2.0 * GOOD_ERROR);
                            rsx! {
                                div { class: if poor { "picture poor" } else { "picture" },
                                    img { src: "{picture.thumbnail}" }
                                    div { class: "caption",
                                        span {
                                            "{index + 1}"
                                            if let Some(error) = error { ": {error:.2} px" }
                                        }
                                        button {
                                            title: "Leave this picture out",
                                            onclick: {
                                                let sessions = sessions.clone();
                                                move |_| sessions.lenses[side.index()].command(LensCommand::Remove(index))
                                            },
                                            "×"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            div { class: "lens-side",
                fieldset {
                    legend { "Fit" }
                    match &session.fit {
                        None => rsx! {
                            p { class: "muted",
                                "Take at least {chessboard::FEWEST_PICTURES} pictures to fit the lens."
                            }
                        },
                        Some(fit) => {
                            let good = calibration::all_passed(&session.checks);
                            let (across, down) = fit.lens.field_of_view();
                            let [k1, k2, k3, k4] = fit.lens.distortion;
                            let record = fit.lens.scaled_to(rig::RECORD_SIZE.0, rig::RECORD_SIZE.1);
                            rsx! {
                                Checks { checks: session.checks.clone() }
                                p { class: if good { "verdict good" } else { "verdict" },
                                    if good {
                                        "Good enough. Use it, or keep going to make it better."
                                    } else {
                                        "Not good enough yet. Keep taking pictures."
                                    }
                                }
                                table { class: "values",
                                    tr { td { "Focal length" } td { "{record.fx:.1} × {record.fy:.1} px" } }
                                    tr { td { "Center" } td { "{record.cx:.1}, {record.cy:.1} px" } }
                                    tr { td { "Distortion" } td { "{k1:.4}, {k2:.4}, {k3:.4}, {k4:.4}" } }
                                    tr { td { "Field of view" } td { "{across:.1}° × {down:.1}°" } }
                                }
                                p { class: "muted", "Measured in {rig::RECORD_SIZE.0} by {rig::RECORD_SIZE.1} recordings." }
                                button {
                                    onclick: {
                                        let (hub, camera, lens) = (hub.clone(), camera.clone(), record.clone());
                                        move |_| calibrator::change_calibration(&hub, &camera, |calibration| {
                                            calibration.cameras[side.index()].lens = lens.clone();
                                        })
                                    },
                                    "Use for the {side_name(side).to_lowercase()} camera"
                                }
                            }
                        }
                    }
                    if let Some(error) = &session.error {
                        p { class: "error", "{error}" }
                    }
                }
                if let Some(straightened) = &session.straightened {
                    fieldset {
                        legend { "Straightened" }
                        img { class: "figure", src: "{straightened.picture}" }
                        if let Some((before, after)) = straightened.bend {
                            p {
                                "In the newest picture the board's rows and columns bend up to {before:.1} px off straight; \
                                 straightened with the fit, {after:.1} px."
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn MountStep(camera: String) -> Element {
    let hub = use_hub();
    let sessions = use_sessions();
    let settings = use_watch(|hub| &hub.settings);
    let stitch = use_channel(&sessions.link.stitch);
    let mut draft = use_signal(|| current_mount(&hub, &camera, &sessions.link));
    let calibrated_here = settings
        .read()
        .cameras
        .get(&camera)
        .is_some_and(|camera| camera.setup.calibration.is_some());
    let current = stitch.read().calibration.cameras.each_ref().map(|camera| camera.orientation);
    let set = move |side: Side, change: fn(&mut Orientation, f64)| {
        move |e: FormEvent| {
            if let Ok(value) = e.value().parse::<f64>() {
                draft.with_mut(|mount| change(&mut mount[side.index()], value));
            }
        }
    };
    rsx! {
        p {
            "How the cameras are meant to point, in degrees: yaw to the right of the rig's forward, pitch up from \
             level, and roll clockwise as seen from behind. Aligning finds how they actually point, keeping as close \
             to this as what they see allows."
        }
        table { class: "orientations",
            tr {
                th {}
                th { "Yaw" }
                th { "Pitch" }
                th { "Roll" }
                th { class: "muted", "Stitched with now" }
            }
            for side in Side::BOTH {
                tr {
                    th { "{side_name(side)}" }
                    td {
                        input {
                            r#type: "number",
                            step: 0.1,
                            value: "{draft.read()[side.index()].yaw}",
                            onchange: set(side, |orientation, value| orientation.yaw = value),
                        }
                    }
                    td {
                        input {
                            r#type: "number",
                            step: 0.1,
                            value: "{draft.read()[side.index()].pitch}",
                            onchange: set(side, |orientation, value| orientation.pitch = value),
                        }
                    }
                    td {
                        input {
                            r#type: "number",
                            step: 0.1,
                            value: "{draft.read()[side.index()].roll}",
                            onchange: set(side, |orientation, value| orientation.roll = value),
                        }
                    }
                    td { class: "muted",
                        {
                            let orientation = current[side.index()];
                            format!("{:.2}, {:.2}, {:.2}", orientation.yaw, orientation.pitch, orientation.roll)
                        }
                    }
                }
            }
        }
        div { class: "toolbar",
            button {
                onclick: {
                    let (hub, camera) = (hub.clone(), camera.clone());
                    move |_| {
                        let mount = draft();
                        hub.update_settings(|s| s.cameras.entry(camera.clone()).or_default().setup.mount = Some(mount));
                    }
                },
                "Save mount"
            }
            button {
                title: "Stitch as if the cameras point exactly as mounted",
                onclick: {
                    let (hub, camera) = (hub.clone(), camera.clone());
                    move |_| {
                        let mount = draft();
                        hub.update_settings(|s| s.cameras.entry(camera.clone()).or_default().setup.mount = Some(mount));
                        calibrator::change_calibration(&hub, &camera, |calibration| {
                            for (camera, orientation) in calibration.cameras.iter_mut().zip(mount) {
                                camera.orientation = orientation;
                            }
                        });
                    }
                },
                "Save and point the cameras as mounted"
            }
        }
        fieldset {
            legend { "Calibration" }
            if calibrated_here {
                p { "Stitching with the calibration made here." }
            } else {
                p { "Stitching with the calibration the camera sends." }
            }
            button {
                disabled: !calibrated_here,
                onclick: {
                    let (hub, camera) = (hub.clone(), camera.clone());
                    move |_| hub.update_settings(|s| s.cameras.entry(camera.clone()).or_default().setup.calibration = None)
                },
                "Go back to the camera's calibration"
            }
        }
    }
}

#[component]
fn AlignStep(camera: String) -> Element {
    let hub = use_hub();
    let runtime = use_runtime();
    let sessions = use_sessions();
    let state = use_channel(&sessions.aligner.state);
    let stitch = use_channel(&sessions.link.stitch);
    let busy = matches!(*state.read(), AlignmentState::Collecting(_) | AlignmentState::Matching);
    let align = {
        let (hub, camera, sessions) = (hub.clone(), camera.clone(), sessions.clone());
        move |_| {
            let mount = current_mount(&hub, &camera, &sessions.link);
            sessions.aligner.start(&runtime, sessions.link.clone(), mount);
        }
    };
    let in_use = |shown: &ShownAlignment| stitch.read().calibration.cameras == shown.alignment.calibration.cameras;
    rsx! {
        p {
            "Point the rig at something with plenty of detail where the cameras' views overlap, far enough away \
             (ten meters or more) that both see it from nearly the same place, and keep it still while the \
             cameras are aligned."
        }
        div { class: "toolbar",
            button { disabled: busy, onclick: align, "Align the cameras" }
        }
        match &*state.read() {
            AlignmentState::Idle => rsx! {},
            AlignmentState::Collecting(collected) => rsx! {
                p { "Collecting previews ({collected} of {ALIGNMENT_PAIRS})…" }
                progress { max: ALIGNMENT_PAIRS as f64, value: *collected as f64 }
            },
            AlignmentState::Matching => rsx! { p { "Matching what the cameras see…" } },
            AlignmentState::Failed(error) => rsx! { p { class: "error", "Couldn't align the cameras: {error}" } },
            AlignmentState::Done(shown) => {
                let good = calibration::all_passed(&shown.checks);
                let alignment = &shown.alignment;
                let used = in_use(shown);
                rsx! {
                    div { class: "alignment",
                        div { class: "alignment-summary",
                            Checks { checks: shown.checks.clone() }
                            p { class: if good { "verdict good" } else { "verdict" },
                                if good { "Good enough." } else { "Not good enough. Try a scene with more detail where the views overlap." }
                            }
                            table { class: "orientations",
                                tr { th {} th { "Before" } th { "Found" } th { "Focal length" } }
                                for side in Side::BOTH {
                                    tr {
                                        th { "{side_name(side)}" }
                                        td { {orientation_text(&shown.before[side.index()])} }
                                        td { {orientation_text(&alignment.calibration.cameras[side.index()].orientation)} }
                                        td { {format!("{:+.2}%", (alignment.focal_scales[side.index()] - 1.0) * 100.0)} }
                                    }
                                }
                            }
                            p { class: "muted",
                                "Yaw, pitch and roll in degrees. The lenses' focal lengths are scaled apart so they agree \
                                 where the views overlap. {alignment.matches} matches over {alignment.pairs.len()} pairs \
                                 of previews, {alignment.inliers} agreeing within {alignment.error:.2} px."
                            }
                            button {
                                disabled: used,
                                onclick: {
                                    let (hub, camera, shown) = (hub.clone(), camera.clone(), shown.clone());
                                    move |_| calibrator::change_calibration(&hub, &camera, |calibration| {
                                        calibration.cameras = shown.alignment.calibration.cameras.clone();
                                    })
                                },
                                if used { "In use" } else { "Use this alignment" }
                            }
                        }
                        div { class: "figures",
                            p { class: "muted",
                                "Where the views overlap, the left camera in red and the right in cyan; they line up where \
                                 it's gray. Nearby things can't line up, since the cameras see them from different places."
                            }
                            div { class: "figure-pair",
                                figure { img { src: "{shown.overlay_before}" } figcaption { "Before" } }
                                figure { img { src: "{shown.overlay_after}" } figcaption { "After" } }
                            }
                            figure {
                                img { src: "{shown.matches}" }
                                figcaption { "Matches between the views: agreeing in green, the rest in red" }
                            }
                            div { class: "figure-pair",
                                figure { img { src: "{shown.features[0]}" } figcaption { "Left camera's features" } }
                                figure { img { src: "{shown.features[1]}" } figcaption { "Right camera's features" } }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn orientation_text(orientation: &Orientation) -> String {
    format!("{:.2}, {:.2}, {:.2}", orientation.yaw, orientation.pitch, orientation.roll)
}

#[component]
fn ViewStep(camera: String) -> Element {
    let hub = use_hub();
    let sessions = use_sessions();
    let stitch = use_channel(&sessions.link.stitch);
    let settings = use_watch(|hub| &hub.settings);
    let view = stitch.read().view;
    let blend = stitch.read().blend;
    let aspect_ratio = stitch.read().aspect_ratio;
    let (record_width, record_height) =
        aspect_ratio.size_with_width(stitch.read().native_width(rig::RECORD_SIZE));
    let (live_width, live_height) = settings.read().live_size(&camera);
    let widest_blur = blend
        .sigmas_for(Blend::REFERENCE_HEIGHT)
        .last()
        .copied()
        .unwrap_or_default();
    let fitted = settings
        .read()
        .cameras
        .get(&camera)
        .is_none_or(|camera| camera.setup.view.is_none());
    let set_view = |change: fn(&mut View, f64)| {
        let (hub, camera) = (hub.clone(), camera.clone());
        move |value: f64| {
            hub.update_settings(|s| {
                let setup = &mut s.cameras.entry(camera.clone()).or_default().setup;
                let mut changed = setup.view.unwrap_or(view);
                change(&mut changed, value);
                setup.view = Some(changed);
            })
        }
    };
    let set_blend = |change: fn(&mut Blend, f64)| {
        let (hub, camera) = (hub.clone(), camera.clone());
        move |value: f64| hub.update_settings(|s| change(&mut s.cameras.entry(camera.clone()).or_default().setup.blend, value))
    };
    rsx! {
        div { class: "view-step",
            div { class: "viewer",
                img { class: "panorama", src: Feed::Panorama.url(&camera) }
            }
            div { class: "view-controls",
                fieldset {
                    legend { "View" }
                    NumberSlider {
                        label: "Yaw",
                        min: -180.0,
                        max: 180.0,
                        step: 0.5,
                        value: view.yaw,
                        decimals: 1,
                        unit: "°",
                        oninput: set_view(|view, value| view.yaw = value),
                    }
                    NumberSlider {
                        label: "Pitch",
                        min: -60.0,
                        max: 60.0,
                        step: 0.5,
                        value: view.pitch,
                        decimals: 1,
                        unit: "°",
                        oninput: set_view(|view, value| view.pitch = value),
                    }
                    NumberSlider {
                        label: "Roll",
                        min: -20.0,
                        max: 20.0,
                        step: 0.1,
                        value: view.roll,
                        decimals: 1,
                        unit: "°",
                        oninput: set_view(|view, value| view.roll = value),
                    }
                    NumberSlider {
                        label: "Across",
                        min: 60.0,
                        max: 300.0,
                        step: 1.0,
                        value: view.horizontal_fov,
                        decimals: 0,
                        unit: "°",
                        oninput: set_view(|view, value| view.horizontal_fov = value),
                    }
                    NumberSlider {
                        label: "Shape",
                        min: AspectRatio::MIN,
                        max: AspectRatio::MAX,
                        step: 0.01,
                        value: aspect_ratio.0,
                        decimals: 2,
                        unit: ":1",
                        oninput: {
                            let (hub, camera) = (hub.clone(), camera.clone());
                            move |value: f64| {
                                hub.update_settings(|s| {
                                    s.cameras.entry(camera.clone()).or_default().setup.aspect_ratio = AspectRatio(value)
                                })
                            }
                        },
                    }
                    p { class: "muted",
                        "Recordings are up to {record_width} by {record_height} px: as fine as the cameras see, if \
                         the encoder takes it. Live previews are {live_width} by {live_height} px; with Live \
                         stream on, set their width in Settings."
                    }
                    button {
                        disabled: fitted,
                        onclick: {
                            let (hub, camera) = (hub.clone(), camera.clone());
                            move |_| hub.update_settings(|s| s.cameras.entry(camera.clone()).or_default().setup.view = None)
                        },
                        "Fit to the cameras"
                    }
                }
                fieldset {
                    legend { "Blend" }
                    NumberSlider {
                        label: "Finest blur",
                        min: 0.0,
                        max: 20.0,
                        step: 0.5,
                        value: blend.finest_sigma,
                        decimals: 1,
                        unit: " px",
                        oninput: set_blend(|blend, value| blend.finest_sigma = value),
                    }
                    NumberSlider {
                        label: "Blur ratio",
                        min: 1.0,
                        max: 4.0,
                        step: 0.05,
                        value: blend.sigma_ratio,
                        decimals: 2,
                        unit: "",
                        oninput: set_blend(|blend, value| blend.sigma_ratio = value),
                    }
                    NumberSlider {
                        label: "Layers",
                        min: 1.0,
                        max: Blend::MAX_LAYERS as f64,
                        step: 1.0,
                        value: blend.layers as f64,
                        decimals: 0,
                        unit: "",
                        oninput: set_blend(|blend, value| blend.layers = value.round() as u32),
                    }
                    p { class: "muted",
                        "Each layer is a band of detail, blended across the seam as blurred as the band is: the \
                         finest by the finest blur, each coarser one by the ratio times more, up to {widest_blur:.0} px. \
                         Fine detail switches cameras sharply while brightness differences even out over the widest \
                         blur. Blurs are given in pixels of 1080p video and scaled for other sizes."
                    }
                    button {
                        disabled: blend == Blend::default(),
                        onclick: {
                            let (hub, camera) = (hub.clone(), camera.clone());
                            move |_| hub.update_settings(|s| s.cameras.entry(camera.clone()).or_default().setup.blend = Blend::default())
                        },
                        "Default blend"
                    }
                }
            }
        }
    }
}