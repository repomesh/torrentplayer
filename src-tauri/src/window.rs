// SPDX-License-Identifier: MIT OR Apache-2.0

//! Showing the main window.
//!
//! The window is created hidden (see `tauri.conf.json`) and shown once the page has loaded, so it
//! never flashes white or half-drawn. It is centered at that point, when its final size at the
//! display's scale is known, rather than from the configured size at creation.
//!
//! While the window is minimized, WebView2 is asked to keep its memory low.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tauri::webview::{PageLoadEvent, PageLoadPayload};
use tauri::{AppHandle, Manager as _, Runtime, Webview, WebviewWindow, Window, WindowEvent};

/// The main window's label.
const MAIN: &str = "main";
/// How long to wait for the page before showing the window anyway.
const FALLBACK: Duration = Duration::from_secs(3);

static SHOWN: AtomicBool = AtomicBool::new(false);
/// Whether the main page has started loading before: later loads are reloads.
static LOADED: AtomicBool = AtomicBool::new(false);

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

/// Shows the main window when its page has finished loading. When the main page loads again (a
/// reload, or after its renderer crashed), ends what the previous page left running: its streams
/// (saving their progress), the explorer's preview, and the best pick's frame. The first load
/// has nothing to end.
pub(crate) fn on_page_load(webview: &Webview, payload: &PageLoadPayload<'_>) {
    if reloading(payload.event(), webview.label(), &LOADED) {
        close_previous_page(webview.app_handle());
    }
    if payload.event() == PageLoadEvent::Finished
        && webview.label() == MAIN
        && let Some(window) = webview.app_handle().get_webview_window(MAIN)
    {
        show(&window);
    }
}

/// Whether this page load event starts a reload of the main page, given whether it `loaded`
/// before (updated).
fn reloading(event: PageLoadEvent, label: &str, loaded: &AtomicBool) -> bool {
    event == PageLoadEvent::Started && label == MAIN && loaded.swap(true, Ordering::SeqCst)
}

/// Ends what a page that is gone left running (see [`on_page_load`]).
fn close_previous_page(app: &AppHandle) {
    tracing::info!("the main page reloaded; stopping what it left running");
    crate::commands::stop_all_streams(app);
    let core = app.state::<crate::core::AppState>().started().cloned();
    crate::preview::stop_preview_of_closed_page(
        &app.state::<crate::preview::PreviewState>(),
        core.as_deref(),
    );
    app.state::<crate::thumbnails::ThumbnailState>()
        .close_page(core.as_deref().map(|core| &core.server));
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

/// Whether the webview was last asked to use little memory (the window minimized).
static LOW_MEMORY: AtomicBool = AtomicBool::new(false);

/// Asks WebView2 to use little memory while the main window is minimized, and normal memory
/// again once it is restored. A minimized window draws nothing, so WebView2 can drop caches it
/// would otherwise keep (about 100 MB at idle); playback and downloads carry on.
pub(crate) fn on_window_event<R: Runtime>(window: &Window<R>, event: &WindowEvent) {
    if window.label() != MAIN || !matches!(event, WindowEvent::Resized(_)) {
        return;
    }
    let low = window.is_minimized().unwrap_or(false);
    if LOW_MEMORY.swap(low, Ordering::SeqCst) == low {
        return;
    }
    if let Some(webview) = window.get_webview_window(MAIN) {
        set_memory_target(&webview, low);
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn set_memory_target<R: Runtime>(window: &WebviewWindow<R>, low: bool) {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL,
        ICoreWebView2_19,
    };
    use windows_core::Interface as _;

    let level = if low {
        COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW
    } else {
        COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL
    };
    let result = window.with_webview(move |webview| {
        // SAFETY: `with_webview` runs this on the main thread with a live controller, which is
        // the thread WebView2's COM objects belong to.
        let core = unsafe { webview.controller().CoreWebView2() };
        // ICoreWebView2_19 needs WebView2 Runtime 114 or later; older ones keep their default.
        let Ok(core) = core.and_then(|core| core.cast::<ICoreWebView2_19>()) else {
            return;
        };
        // SAFETY: as above; `level` is one of the two documented values.
        if let Err(e) = unsafe { core.SetMemoryUsageTargetLevel(level) } {
            tracing::debug!(error = %e, "could not set the webview's memory target");
        }
    });
    if let Err(e) = result {
        tracing::debug!(error = %e, "could not reach the webview");
    }
}

#[cfg(not(windows))]
fn set_memory_target<R: Runtime>(_window: &WebviewWindow<R>, _low: bool) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_later_loads_of_the_main_page_are_reloads() {
        let loaded = AtomicBool::new(false);
        assert!(
            !reloading(PageLoadEvent::Started, MAIN, &loaded),
            "the first load"
        );
        assert!(!reloading(PageLoadEvent::Finished, MAIN, &loaded));
        assert!(!reloading(PageLoadEvent::Started, "other", &loaded));
        assert!(reloading(PageLoadEvent::Started, MAIN, &loaded));
        assert!(!reloading(PageLoadEvent::Finished, MAIN, &loaded));
        assert!(reloading(PageLoadEvent::Started, MAIN, &loaded));
    }
}
