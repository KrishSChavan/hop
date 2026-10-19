//! The Hop app (docs/M6-APP.md): one window to pair the two computers, say
//! where the other one sits, and start and stop sharing.
//!
//! - `model`: what the window shows, and the rules for how it changes.
//! - `view`: draws the model, and turns clicks into actions.
//! - `app`: carries the actions out, with the agent and pairing threads
//!   from hop-agent behind it.
//! - `bridge`: brings their news (and the log's status lines) to the
//!   window's thread.
//! - `launch`: on a Mac, hands `hop` subcommands to the command line.
//!
//! The rules of the `hop` command hold here too: nothing logs keys,
//! clipboard text, pairing codes, secrets or file names (the names of files
//! that arrived are shown in Activity only), and every way out (Stop
//! sharing, closing the window, Cmd+Q, a quit signal on a Mac, a panic)
//! gives the keyboard and mouse back first.

// A windowed program on Windows: no console window opens with it. (Its
// test binary stays a console program, so `cargo test` prints as usual.)
#![cfg_attr(not(test), windows_subsystem = "windows")]
#![forbid(unsafe_code)]

mod app;
mod bridge;
mod launch;
mod model;
mod view;

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{self, FontId, TextStyle};
use hop_agent::{Os, PermissionState, logging, service, settings, status, system};

use crate::app::HopApp;
use crate::launch::Launch;
use crate::model::{Model, Settings, words};

/// The window's size when it opens, in points.
const WINDOW_SIZE: [f32; 2] = [480.0, 680.0];

/// The smallest the window can be made.
const WINDOW_MIN: [f32; 2] = [380.0, 420.0];

/// Hop's icon, drawn by scripts/dev/make-icon.py: the window's icon on
/// Windows (title bar and taskbar), and the Dock's on a Mac.
const ICON_PNG: &[u8] = include_bytes!("../../../assets/icon/hop-256.png");

/// How long the log gets for its last lines after the window closed.
const LOG_FLUSH: Duration = Duration::from_secs(1);

fn main() -> anyhow::Result<ExitCode> {
    // On a Mac, Hop.app's executable is this program; the scripts start it
    // through LaunchServices with `--args run` or `--args selftest`.
    if cfg!(target_os = "macos")
        && launch::launch(std::env::args_os().nth(1).as_deref()) == Launch::CommandLine
    {
        return hop_agent::cli::main();
    }
    window()
}

/// Opens the window and runs until it closes.
fn window() -> anyhow::Result<ExitCode> {
    let shared = bridge::Shared::new();
    // One log for the whole app: hop-app.log, never echoed anywhere; its
    // status lines also go to the Activity list.
    let log = match logging::start_app_log(bridge::tap(Arc::clone(&shared))) {
        Ok((log, _file)) => Some(log),
        Err(error) => {
            shared.push_line(&format!(
                "Could not start Hop's log ({error}), so this session is not written to a file."
            ));
            None
        }
    };
    // Any panic, on any thread, ends sharing failing open. This is the main
    // thread, where a panic also waits for sharing to have stopped.
    service::install_panic_hook(log.clone());
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "the Hop app started");
    // On a Mac, `kill`, or Ctrl+C or closing the terminal that started Hop,
    // closes the window the usual way: input back, then goodbye.
    let signalled = Arc::clone(&shared);
    let listening = service::on_quit_signal(move || match signalled.window() {
        Some(ctx) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
        // No window yet, so nothing runs that needs stopping.
        None => std::process::exit(130),
    });
    if let Err(error) = listening {
        tracing::warn!(error = %error, "could not listen for requests to quit");
    }
    // "About Hop" and "Quit Hop" in the Mac's app menu, not "hop".
    system::use_app_name();

    let permission = match system::accessibility() {
        PermissionState::NotNeeded => None,
        state => Some(state),
    };
    let model = Model::new(Os::current(), load_settings(), permission);
    let app_log = log.clone();
    let result = eframe::run_native(
        words::HOP,
        native_options(),
        Box::new(move |cc| {
            style(&cc.egui_ctx);
            shared.set_window(&cc.egui_ctx);
            Ok(Box::new(HopApp::new(model, shared, app_log)))
        }),
    );
    let code = match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            status!("Could not open Hop's window ({error}).");
            ExitCode::FAILURE
        }
    };
    if let Some(log) = &log {
        log.flush(LOG_FLUSH);
    }
    Ok(code)
}

/// The settings at startup (making the key and the config on a first run).
/// What loading has to say goes to the log and the Activity list; a failure
/// shows at the top of the window instead.
fn load_settings() -> Settings {
    match settings::overview() {
        Ok(overview) => {
            for note in &overview.notes {
                status!("{note}");
            }
            Settings::Loaded(overview)
        }
        Err(error) => {
            let error = format!("{error:#}");
            tracing::error!(error = %error, "could not read the settings");
            Settings::Failed(error)
        }
    }
}

/// One window titled "Hop", about 480×680 points, resizable, with Hop's
/// icon, in the middle of the screen.
fn native_options() -> eframe::NativeOptions {
    let viewport = egui::ViewportBuilder::default()
        .with_title(words::HOP)
        .with_inner_size(WINDOW_SIZE)
        .with_min_inner_size(WINDOW_MIN)
        .with_resizable(true)
        .with_icon(icon());
    eframe::NativeOptions {
        viewport,
        centered: true,
        renderer: renderer(),
        event_loop_builder: event_loop_builder(),
        ..Default::default()
    }
}

/// On a Mac, the window makes Hop a regular app: a Dock icon, the app menu,
/// a place in the app switcher. Hop.app's Info.plist says LSUIElement (an
/// app with no Dock icon), so the `hop` commands the scripts start through
/// LaunchServices (`run`, `selftest`) never show in the Dock; winit asks
/// for the regular policy only when told to.
#[cfg(target_os = "macos")]
fn event_loop_builder() -> Option<eframe::EventLoopBuilderHook> {
    use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS as _};

    Some(Box::new(|builder| {
        builder.with_activation_policy(ActivationPolicy::Regular);
    }))
}

/// Elsewhere winit's defaults are right.
#[cfg(not(target_os = "macos"))]
fn event_loop_builder() -> Option<eframe::EventLoopBuilderHook> {
    None
}

/// The wgpu renderer (Metal on a Mac, Direct3D 12 or Vulkan on Windows).
///
/// A development build with the `screenshot` feature uses eframe's OpenGL
/// renderer instead when EFRAME_SCREENSHOT_TO names a `.png` file: only
/// that renderer saves the first frame there (and then exits).
fn renderer() -> eframe::Renderer {
    #[cfg(feature = "screenshot")]
    if std::env::var_os("EFRAME_SCREENSHOT_TO").is_some() {
        return eframe::Renderer::Glow;
    }
    eframe::Renderer::Wgpu
}

/// Hop's icon. A picture that cannot be read means no icon (not egui's).
fn icon() -> egui::IconData {
    eframe::icon_data::from_png_bytes(ICON_PNG).unwrap_or_default()
}

/// Text a little larger than egui's defaults, for reading at a glance.
fn style(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        for (text_style, size) in [
            (TextStyle::Small, 11.5),
            (TextStyle::Body, 14.0),
            (TextStyle::Button, 14.0),
            (TextStyle::Heading, 20.0),
        ] {
            style
                .text_styles
                .insert(text_style, FontId::proportional(size));
        }
        style
            .text_styles
            .insert(TextStyle::Monospace, FontId::monospace(13.0));
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(8.0, 4.0);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_icon_is_a_square_picture() {
        let icon = eframe::icon_data::from_png_bytes(ICON_PNG).expect("hop-256.png decodes");
        assert_eq!((icon.width, icon.height), (256, 256));
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
    }

    #[test]
    fn the_window_opens_at_the_size_the_spec_gives() {
        let options = native_options();
        assert_eq!(options.viewport.title.as_deref(), Some("Hop"));
        assert_eq!(
            options.viewport.inner_size,
            Some(egui::vec2(WINDOW_SIZE[0], WINDOW_SIZE[1]))
        );
        assert_eq!(options.viewport.resizable, Some(true));
        assert!(options.viewport.icon.is_some_and(|icon| icon.width == 256));
    }

    #[test]
    fn the_style_keeps_every_text_style() {
        let ctx = egui::Context::default();
        style(&ctx);
        let style = ctx.global_style();
        for text_style in [
            TextStyle::Small,
            TextStyle::Body,
            TextStyle::Button,
            TextStyle::Heading,
            TextStyle::Monospace,
        ] {
            assert!(
                style.text_styles.contains_key(&text_style),
                "{text_style:?}"
            );
        }
    }
}
