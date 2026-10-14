//! The desktop UI. Components render the hub's state and call into it for
//! changes; all I/O happens in the backend.
//!
//! The window is split into panes, opened and closed from the Window menu:
//! Cameras on the left, Settings and Jobs stacked on the right.

mod cameras;
mod jobs;
mod settings;

use std::cell::RefCell;
use std::sync::Arc;

use dioxus::desktop::muda::{CheckMenuItem, Menu, MenuEvent, PredefinedMenuItem, Submenu};
use dioxus::desktop::tao::window::Icon;
use dioxus::desktop::wry::http::{Request, Response, StatusCode, header};
use dioxus::desktop::{
    Config, LogicalSize, RequestAsyncResponder, WindowBuilder, icon_from_memory,
    use_muda_event_handler,
};
use dioxus::prelude::*;
use stitching::JobStatus;
use tokio::runtime::Handle;
use tokio::sync::watch;

use crate::hub::Hub;
use crate::settings::{Align, Panes, Stitching};

const STYLE: &str = include_str!("style.css");
const ICON: &[u8] = include_bytes!("../../assets/icon.png");
/// Custom URL scheme proxying the Arctos web app, so its pages can be shown
/// in the window (see [`proxy_arctos`]).
const ARCTOS_SCHEME: &str = "arctos";

thread_local! {
    /// The Window menu's check items, by pane.
    static PANE_ITEMS: RefCell<Vec<(Pane, CheckMenuItem)>> = const { RefCell::new(Vec::new()) };
}

pub fn launch(hub: Arc<Hub>, runtime: Handle) {
    let window = WindowBuilder::new()
        .with_title("Arctos Recorder")
        .with_inner_size(LogicalSize::new(1280.0, 900.0));
    let icon: Icon = icon_from_memory(ICON).expect("valid icon");
    let config = Config::new()
        .with_window(window)
        .with_icon(icon)
        .with_menu(menu(hub.settings.borrow().panes))
        .with_background_color((0xF4, 0xF6, 0xD7, 0xFF))
        .with_asynchronous_custom_protocol(ARCTOS_SCHEME, proxy_arctos(hub.clone(), runtime));
    dioxus::LaunchBuilder::desktop()
        .with_cfg(config)
        .with_context(hub)
        .launch(App);
}

fn menu(panes: Panes) -> Menu {
    let file =
        Submenu::with_items("File", true, &[&PredefinedMenuItem::quit(None)]).expect("build menu");
    let edit = Submenu::with_items(
        "Edit",
        true,
        &[
            &PredefinedMenuItem::cut(None),
            &PredefinedMenuItem::copy(None),
            &PredefinedMenuItem::paste(None),
            &PredefinedMenuItem::select_all(None),
        ],
    )
    .expect("build menu");
    let window = Submenu::new("Window", true);
    PANE_ITEMS.with_borrow_mut(|items| {
        for pane in Pane::ALL {
            let item = CheckMenuItem::with_id(
                pane.menu_id(),
                pane.title(),
                true,
                pane.is_open(&panes),
                None,
            );
            window.append(&item).expect("build menu");
            items.push((pane, item));
        }
    });
    Menu::with_items(&[&file, &edit, &window]).expect("build menu")
}

/// Serves `arctos://<align>/<path>` from the Arctos server (see
/// [`arctos_url`]). Loading Arctos pages directly would open them in the
/// browser (Dioxus sends every http(s) navigation there) and would need the
/// Cloudflare Access headers.
fn proxy_arctos(
    hub: Arc<Hub>,
    runtime: Handle,
) -> impl Fn(dioxus::desktop::wry::WebViewId, Request<Vec<u8>>, RequestAsyncResponder) + 'static {
    move |_, request, responder| {
        let hub = hub.clone();
        let align = match request.uri().host() {
            Some("center") => Align::Center,
            Some("end") => Align::End,
            _ => Align::Start,
        };
        let path = request
            .uri()
            .path_and_query()
            .map_or("/", |path| path.as_str())
            .to_owned();
        runtime.spawn(async move {
            let response = match hub.arctos.page(&path).await {
                Ok(page) => {
                    let mut response = Response::builder().status(page.status.as_u16());
                    let mut body = page.body.to_vec();
                    if let Some(content_type) = page.content_type {
                        if content_type.as_bytes().starts_with(b"text/html") {
                            body = align_page(&body, align);
                        }
                        response = response.header(header::CONTENT_TYPE, content_type.as_bytes());
                    }
                    response.body(body)
                }
                Err(e) => Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(format!("{e:#}").into_bytes()),
            };
            responder.respond(response.expect("valid response"));
        });
    }
}

/// URL of an Arctos page (path and query) for showing it in the window, with
/// its content pinned to the top, center, or bottom of the frame. The page
/// can't be measured from outside, so this is how a scoreboard overlay
/// anchored at the bottom stays at the bottom whatever its height.
fn arctos_url(path: &str, align: Align) -> String {
    let host = match align {
        Align::Start => "start",
        Align::Center => "center",
        Align::End => "end",
    };
    format!("{ARCTOS_SCHEME}://{host}{path}")
}

/// Pins the Arctos web app's root element to the top, center, or bottom of
/// the page.
fn align_page(html: &[u8], align: Align) -> Vec<u8> {
    let justify = match align {
        Align::Start => return html.to_vec(),
        Align::Center => "center",
        Align::End => "flex-end",
    };
    let style = format!(
        "<style>#main {{ position: fixed; inset: 0; display: flex; \
         flex-direction: column; justify-content: {justify}; }}</style></head>"
    );
    String::from_utf8_lossy(html)
        .replacen("</head>", &style, 1)
        .into_bytes()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pane {
    Cameras,
    Jobs,
    Settings,
}

impl Pane {
    const ALL: [Pane; 3] = [Pane::Cameras, Pane::Jobs, Pane::Settings];

    fn title(self) -> &'static str {
        match self {
            Pane::Cameras => "Cameras",
            Pane::Jobs => "Jobs",
            Pane::Settings => "Settings",
        }
    }

    fn menu_id(self) -> &'static str {
        match self {
            Pane::Cameras => "pane-cameras",
            Pane::Jobs => "pane-jobs",
            Pane::Settings => "pane-settings",
        }
    }

    fn open_flag(self, panes: &mut Panes) -> &mut bool {
        match self {
            Pane::Cameras => &mut panes.cameras,
            Pane::Jobs => &mut panes.jobs,
            Pane::Settings => &mut panes.settings,
        }
    }

    fn is_open(self, panes: &Panes) -> bool {
        match self {
            Pane::Cameras => panes.cameras,
            Pane::Jobs => panes.jobs,
            Pane::Settings => panes.settings,
        }
    }
}

fn open_count(panes: &Panes) -> usize {
    Pane::ALL.iter().filter(|p| p.is_open(panes)).count()
}

/// Opens or closes a pane, except that the last open pane stays open.
fn set_pane(hub: &Hub, pane: Pane, open: bool) {
    hub.update_settings(|s| {
        if open || open_count(&s.panes) > 1 {
            *pane.open_flag(&mut s.panes) = open;
        }
    });
    // The menu toggles its check mark by itself, so put it right if the
    // change was refused.
    let panes = hub.settings.borrow().panes;
    PANE_ITEMS.with_borrow(|items| {
        for (pane, item) in items {
            item.set_checked(pane.is_open(&panes));
        }
    });
}

#[component]
fn App() -> Element {
    let hub = use_hub();
    let settings = use_watch(|hub| &hub.settings);
    use_muda_event_handler({
        let hub = hub.clone();
        move |event: &MenuEvent| {
            if let Some(pane) = Pane::ALL.into_iter().find(|p| event.id == p.menu_id()) {
                let open = pane.is_open(&hub.settings.borrow().panes);
                set_pane(&hub, pane, !open);
            }
        }
    });

    let panes = settings.read().panes;
    let closable = open_count(&panes) > 1;
    rsx! {
        style { {STYLE} }
        div { class: "workspace",
            if panes.cameras {
                PaneFrame { pane: Pane::Cameras, closable, cameras::Cameras {} }
            }
            if panes.settings || panes.jobs {
                div { class: if panes.cameras { "side" } else { "side wide" },
                    if panes.settings {
                        PaneFrame { pane: Pane::Settings, closable, settings::Settings {} }
                    }
                    if panes.jobs {
                        PaneFrame { pane: Pane::Jobs, closable, jobs::Jobs {} }
                    }
                }
            }
        }
        StatusBar {}
    }
}

#[component]
fn PaneFrame(pane: Pane, closable: bool, children: Element) -> Element {
    let hub = use_hub();
    let class = format!("pane {}", pane.title().to_lowercase());
    rsx! {
        section { class,
            div { class: "pane-title",
                span { "{pane.title()}" }
                if closable {
                    button {
                        class: "close",
                        title: "Close pane",
                        onclick: move |_| set_pane(&hub, pane, false),
                        "×"
                    }
                }
            }
            div { class: "pane-body", {children} }
        }
    }
}

#[component]
fn StatusBar() -> Element {
    let hub = use_hub();
    let cameras = use_watch(|hub| &hub.cameras);
    let settings = use_watch(|hub| &hub.settings);
    let arctos_fields = use_watch(|hub| &hub.arctos_fields);
    let jobs = use_watch(|hub| &hub.jobs);

    let online = cameras.read().values().filter(|c| c.online).count();
    let stitching = match settings.read().stitching {
        Stitching::Local => "local",
        Stitching::Cloud => "cloud",
    };
    let active = jobs
        .read()
        .iter()
        .filter(|job| !matches!(job.status, JobStatus::Done | JobStatus::Failed(_)))
        .count();
    let failed = jobs
        .read()
        .iter()
        .filter(|job| matches!(job.status, JobStatus::Failed(_)))
        .count();
    let arctos_error = arctos_fields.read().clone().err();
    rsx! {
        footer { class: "statusbar",
            span { "Cameras online: {online}" }
            span { "Stitching: {stitching}" }
            span { "Jobs: {active} active" if failed > 0 { ", {failed} failed" } }
            span {
                class: if arctos_error.is_some() { "error" },
                title: arctos_error.clone().unwrap_or_else(|| "Set ARCTOS_URL to change".into()),
                "Arctos: {hub.arctos.url()}"
                if arctos_error.is_some() { " (can't load the tournament)" }
            }
        }
    }
}

fn use_hub() -> Arc<Hub> {
    use_context()
}

/// Mirrors one of the hub's channels into a signal.
fn use_watch<T: Clone + 'static>(channel: fn(&Hub) -> &watch::Sender<T>) -> Signal<T> {
    let hub = use_hub();
    let mut value = use_signal(|| channel(&hub).borrow().clone());
    use_future(move || {
        let mut receiver = channel(&hub).subscribe();
        async move {
            while receiver.changed().await.is_ok() {
                value.set(receiver.borrow_and_update().clone());
            }
        }
    });
    value
}
