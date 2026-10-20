// SPDX-License-Identifier: MIT OR Apache-2.0

//! Showing the main window.
//!
//! The window is created hidden (see `tauri.conf.json`) and shown once the page has loaded, so it
//! never flashes white or half-drawn. It is centered at that point, when its final size at the
//! display's scale is known, rather than from the configured size at creation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tauri::webview::{PageLoadEvent, PageLoadPayload};
use tauri::{AppHandle, Manager as _, Runtime, Webview, WebviewWindow};

/// The main window's label.
const MAIN: &str = "main";
/// How long to wait for the page before showing the window anyway.
const FALLBACK: Duration = Duration::from_secs(3);

static SHOWN: AtomicBool = AtomicBool::new(false);

/// Centers and shows the main window, once.
fn show<R: Runtime>(window: &WebviewWindow<R>) {
    if SHOWN.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Err(e) = window.center() {
        tracing::debug!(error = %e, "could not center the window");
    }
    if let Err(e) = window.show() {
        tracing::warn!(error = %e, "could not show the window");
    }
    let _ = window.set_focus();
}

/// Shows the main window when its page has finished loading.
pub(crate) fn on_page_load<R: Runtime>(webview: &Webview<R>, payload: &PageLoadPayload<'_>) {
    if payload.event() == PageLoadEvent::Finished
        && webview.label() == MAIN
        && let Some(window) = webview.app_handle().get_webview_window(MAIN)
    {
        show(&window);
    }
}

/// Shows the main window after a while even if its page never reports that it loaded, so a
/// webview problem can't leave the app running invisibly.
pub(crate) fn show_eventually<R: Runtime>(app: &AppHandle<R>) {
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(FALLBACK);
        if let Some(window) = app.get_webview_window(MAIN) {
            let shown = window.clone();
            let _ = window.run_on_main_thread(move || show(&shown));
        }
    });
}
