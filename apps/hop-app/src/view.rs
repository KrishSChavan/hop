//! Draws the window from the model, top to bottom (docs/M6-APP.md, "The
//! window"): the header, Sharing, Your other computer, Shortcuts, Pair,
//! Permissions (on a Mac) and Activity, plus the two dialogs.
//!
//! Drawing changes nothing but the text fields: every click becomes an
//! [`Action`] for the app to carry out after the frame. So the view can be
//! drawn in a test with any state, without a window or a network.

use std::time::Instant;

use eframe::egui::{
    self, Button, Color32, Id, Key, Label, Modal, RadioButton, RichText, ScrollArea, Sense, Stroke,
    TextEdit, TextStyle, Ui, pos2, vec2,
};
use hop_agent::settings::PeerView;
use hop_agent::{CmdCtrl, DeviceId, PermissionState, Side};

use crate::model::pairing::{self, Joining, Outcome, Pairing, Showing, Stage};
use crate::model::sharing::Tone;
use crate::model::{Model, Settings, words};

/// What the user asked for, for the app to carry out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// "Start sharing".
    StartSharing,
    /// "Stop sharing".
    StopSharing,
    /// One of the four side buttons.
    SetSide { id: DeviceId, side: Side },
    /// "Save address", or Return in the Address field.
    SaveAddress(DeviceId),
    /// "Forget…": opens the dialog.
    AskForget(DeviceId),
    /// The dialog's "Cancel".
    CancelForget,
    /// The dialog's "Forget".
    Forget(DeviceId),
    /// One of the two Shortcuts choices, when it is not the one in use.
    SetShortcuts(CmdCtrl),
    /// "Show a pairing code".
    ShowCode,
    /// "Enter a code": opens the form.
    OpenJoin,
    /// The form's "Cancel" while nothing runs.
    CloseJoin,
    /// The form's "Pair", or Return in the Code field.
    Join,
    /// "Cancel" while a pairing session runs.
    CancelPairing,
    /// "Yes, pair" or "No".
    Answer(bool),
    /// "Allow…": macOS's prompt and the Accessibility settings.
    AllowAccessibility,
    /// "Open log folder".
    OpenLogFolder,
    /// "Try again" after the settings could not be read.
    RetrySettings,
}

/// Grey: not sharing.
const IDLE: Color32 = Color32::from_rgb(0x8c, 0x95, 0x9f);
/// Amber: starting, stopping, looking for the other computer.
const BUSY: Color32 = Color32::from_rgb(0xd9, 0x8e, 0x04);
/// Green: connected, allowed, paired.
const GOOD: Color32 = Color32::from_rgb(0x2e, 0xa0, 0x43);
/// Blue: one keyboard and mouse controls the other computer.
const ACTIVE: Color32 = Color32::from_rgb(0x2f, 0x81, 0xf7);
/// Red: stopped by an error.
const PROBLEM: Color32 = Color32::from_rgb(0xd1, 0x24, 0x2f);

/// The Code field's id, so its undo history can be found and dropped.
pub fn code_field_id() -> Id {
    Id::unique("hop-pair-code")
}

/// Draws the whole window for one frame.
pub fn show(ui: &mut Ui, model: &mut Model, now: Instant, actions: &mut Vec<Action>) {
    egui::CentralPanel::default().show(ui, |ui| {
        ScrollArea::vertical()
            .id_salt("hop-window")
            .auto_shrink(false)
            .show(ui, |ui| {
                header(ui, model, actions);
                section(ui, words::SHARING);
                sharing(ui, model, actions);
                section(ui, words::YOUR_OTHER_COMPUTER);
                peers(ui, model, actions);
                section(ui, words::SHORTCUTS);
                shortcuts(ui, model, actions);
                section(ui, words::PAIRING);
                pair(ui, model, now, actions);
                if let Some(state) = model.permission {
                    section(ui, words::PERMISSIONS);
                    permissions(ui, state, actions);
                }
                section(ui, words::ACTIVITY);
                activity(ui, model, actions);
            });
    });
    let ctx = ui.ctx().clone();
    question_dialog(&ctx, &model.pairing, now, actions);
    forget_dialog(&ctx, model, actions);
}

/// "Hop" and the version, then this computer; a settings problem right
/// under it, where it cannot be missed.
fn header(ui: &mut Ui, model: &Model, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(words::HOP).size(26.0).strong());
        ui.label(RichText::new(format!("version {}", env!("CARGO_PKG_VERSION"))).weak());
    });
    match &model.settings {
        Settings::Loaded(overview) => {
            ui.label(words::this_computer(&overview.name, model.os));
        }
        Settings::Failed(error) => {
            ui.add_space(6.0);
            egui::Frame::group(ui.style())
                .stroke(Stroke::new(1.5, PROBLEM))
                .show(ui, |ui| {
                    ui.add(
                        Label::new(RichText::new(words::settings_failed(error)).color(PROBLEM))
                            .wrap(),
                    );
                    if ui
                        .add_enabled(!model.quitting, Button::new(words::TRY_AGAIN))
                        .clicked()
                    {
                        actions.push(Action::RetrySettings);
                    }
                });
        }
    }
}

/// A section's title, with some air and a line above it.
fn section(ui: &mut Ui, title: &str) {
    ui.add_space(10.0);
    ui.separator();
    ui.label(RichText::new(title).size(17.0).strong());
    ui.add_space(2.0);
}

/// Text that wraps at the window's edge.
fn wrapped(ui: &mut Ui, text: impl Into<egui::WidgetText>) {
    ui.add(Label::new(text).wrap());
}

/// The status dot and line, the button, and the hints under it.
fn sharing(ui: &mut Ui, model: &Model, actions: &mut Vec<Action>) {
    let (tone, status) = model.sharing.status();
    ui.horizontal_top(|ui| {
        dot(ui, tone);
        wrapped(ui, status);
    });
    ui.add_space(4.0);
    let (label, enabled) = model.sharing_button();
    let button = Button::new(RichText::new(label).strong()).min_size(vec2(150.0, 30.0));
    if ui.add_enabled(enabled, button).clicked() {
        actions.push(if model.sharing.is_active() {
            Action::StopSharing
        } else {
            Action::StartSharing
        });
    }
    for hint in model.sharing_hints() {
        wrapped(ui, hint);
    }
}

/// The coloured dot, drawn rather than typed, so it never depends on a
/// font having the glyph.
fn dot(ui: &mut Ui, tone: Tone) {
    let size = ui.text_style_height(&TextStyle::Body);
    let (rect, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    let color = match tone {
        Tone::Idle => IDLE,
        Tone::Busy => BUSY,
        Tone::Ready => GOOD,
        Tone::Active => ACTIVE,
        Tone::Problem => PROBLEM,
    };
    ui.painter()
        .circle_filled(rect.center(), size * 0.32, color);
}

/// A green tick, drawn like the dot.
fn tick(ui: &mut Ui) {
    let size = ui.text_style_height(&TextStyle::Body);
    let (rect, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    let r = rect.shrink(size * 0.15);
    let points = vec![
        pos2(r.left(), r.center().y),
        pos2(r.left() + r.width() * 0.38, r.bottom() - r.height() * 0.1),
        pos2(r.right(), r.top() + r.height() * 0.1),
    ];
    ui.painter().line(points, Stroke::new(2.0, GOOD));
}

/// "Your other computer": each paired computer, with its side, address and
/// Forget.
fn peers(ui: &mut Ui, model: &mut Model, actions: &mut Vec<Action>) {
    if !model.settings_loaded() {
        wrapped(ui, RichText::new(words::PEERS_UNKNOWN).weak());
        return;
    }
    let peers: Vec<PeerView> = model.paired_peers().cloned().collect();
    if peers.is_empty() {
        wrapped(ui, words::NOT_PAIRED_YET);
        return;
    }
    for (index, peer) in peers.iter().enumerate() {
        if index > 0 {
            ui.add_space(8.0);
        }
        ui.push_id(peer.id.as_str(), |ui| peer_card(ui, model, peer, actions));
    }
}

fn peer_card(ui: &mut Ui, model: &mut Model, peer: &PeerView, actions: &mut Vec<Action>) {
    let enabled = !model.quitting;
    ui.label(RichText::new(words::computer(&peer.name, peer.os)).strong());
    ui.horizontal_wrapped(|ui| {
        ui.label(words::WHERE_IS_IT);
        for side in [Side::Left, Side::Right, Side::Above, Side::Below] {
            let button = Button::selectable(peer.side == side, words::side_label(side));
            if ui.add_enabled(enabled, button).clicked() && peer.side != side {
                actions.push(Action::SetSide {
                    id: peer.id.clone(),
                    side,
                });
            }
        }
    });
    wrapped(
        ui,
        RichText::new(words::choose_there(&peer.name, peer.side)).weak(),
    );
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        ui.label(words::ADDRESS);
        ui.label(RichText::new(format!("({})", words::ADDRESS_NOTE)).weak());
    });
    ui.horizontal(|ui| {
        let field = model.peers.addresses.entry(peer.id.clone()).or_default();
        let edit = ui.add_enabled(
            enabled,
            TextEdit::singleline(field)
                .hint_text(words::ADDRESS_HINT)
                .desired_width(170.0),
        );
        let entered = edit.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter));
        let save = ui
            .add_enabled(enabled, Button::new(words::SAVE_ADDRESS))
            .clicked();
        if entered || save {
            actions.push(Action::SaveAddress(peer.id.clone()));
        }
    });
    if let Some((id, problem)) = &model.peers.problem
        && *id == peer.id
    {
        wrapped(ui, RichText::new(problem).color(PROBLEM));
    }
    if ui
        .add_enabled(enabled, Button::new(words::FORGET_ELLIPSIS))
        .clicked()
    {
        actions.push(Action::AskForget(peer.id.clone()));
    }
}

/// "Shortcuts": what Cmd and Ctrl do with the other computer's keyboard on
/// this one. Each computer applies its own choice to the keys that arrive
/// there, so the words name the other computer's keyboard.
fn shortcuts(ui: &mut Ui, model: &Model, actions: &mut Vec<Action>) {
    let Some(current) = model.cmd_ctrl() else {
        wrapped(ui, RichText::new(words::SHORTCUTS_UNKNOWN).weak());
        return;
    };
    let other = model.other_computer();
    wrapped(ui, words::with_keyboard(other));
    for (choice, label) in words::shortcut_choices(model.os) {
        let clicked = ui
            .add_enabled(!model.quitting, RadioButton::new(current == choice, label))
            .clicked();
        if clicked && current != choice {
            actions.push(Action::SetShortcuts(choice));
        }
    }
    wrapped(ui, RichText::new(words::this_keyboard_there(other)).weak());
    if let Some(problem) = &model.shortcuts_problem {
        wrapped(ui, RichText::new(problem).color(PROBLEM));
    }
}

/// "Pair": the two buttons, then whichever way of pairing is under way,
/// then how the last try ended.
fn pair(ui: &mut Ui, model: &mut Model, now: Instant, actions: &mut Vec<Action>) {
    let can_pair = model.can_pair();
    let joining = matches!(model.pairing.stage, Stage::Join(_));
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(can_pair, Button::new(words::SHOW_CODE))
            .clicked()
        {
            actions.push(Action::ShowCode);
        }
        if ui
            .add_enabled(can_pair && !joining, Button::new(words::ENTER_CODE))
            .clicked()
        {
            actions.push(Action::OpenJoin);
        }
    });
    let quitting = model.quitting;
    let pairing = &mut model.pairing;
    if joining {
        join_form(ui, pairing, quitting, actions);
    } else if let Stage::Show(showing) = &pairing.stage {
        show_code(ui, showing, pairing.cancelling, now, actions);
    }
    if let Some(outcome) = &pairing.outcome {
        outcome_lines(ui, outcome);
    }
}

/// The code, large, with how to use it and the time it has left.
fn show_code(
    ui: &mut Ui,
    showing: &Showing,
    cancelling: bool,
    now: Instant,
    actions: &mut Vec<Action>,
) {
    ui.add_space(6.0);
    if let Some(line) = &showing.not_announced {
        wrapped(ui, RichText::new(line).color(BUSY));
    }
    match (&showing.code, showing.expires) {
        (Some(code), Some(expires)) => {
            // On screen only. Not selectable, so a stray drag cannot copy it
            // to the clipboard.
            ui.add(
                Label::new(RichText::new(code.as_str()).size(44.0).monospace().strong())
                    .selectable(false),
            );
            wrapped(ui, words::CODE_HELP);
            ui.label(words::time_left(pairing::left(expires, now)));
        }
        _ => {
            ui.label(RichText::new(words::GETTING_CODE).weak());
        }
    }
    if showing.wrong > 0 {
        wrapped(
            ui,
            RichText::new(words::wrong_code(showing.wrong)).color(BUSY),
        );
    }
    if let Some(name) = &showing.accepted {
        wrapped(ui, words::pairing_with(name));
    }
    let label = if cancelling {
        words::CANCELLING
    } else {
        words::CANCEL
    };
    if ui.add_enabled(!cancelling, Button::new(label)).clicked() {
        actions.push(Action::CancelPairing);
    }
}

/// The "Enter a code" form, with its progress.
fn join_form(ui: &mut Ui, pairing: &mut Pairing, quitting: bool, actions: &mut Vec<Action>) {
    let busy = pairing.busy;
    let can_join = pairing.can_join() && !quitting;
    ui.add_space(6.0);
    ui.label(words::CODE);
    // The field edits the zeroizing buffer itself; its room is reserved, so
    // typing never moves the code to a new, unwiped buffer.
    let code = ui.add_enabled(
        !busy && !quitting,
        TextEdit::singleline(&mut *pairing.code)
            .id(code_field_id())
            .char_limit(pairing::CODE_FIELD_CHARS)
            .hint_text(words::CODE_HINT)
            .desired_width(140.0),
    );
    let entered = code.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter));
    ui.add_space(4.0);
    ui.label(RichText::new(words::NOT_FOUND).weak());
    ui.horizontal(|ui| {
        ui.label(words::ADDRESS);
        ui.add_enabled(
            !busy && !quitting,
            TextEdit::singleline(&mut pairing.address)
                .hint_text(words::ADDRESS_HINT)
                .desired_width(170.0),
        );
    });
    ui.horizontal(|ui| {
        let pair = ui.add_enabled(can_join, Button::new(words::PAIR)).clicked();
        if pair || (entered && can_join) {
            actions.push(Action::Join);
        }
        let label = if pairing.cancelling {
            words::CANCELLING
        } else {
            words::CANCEL
        };
        if ui
            .add_enabled(!pairing.cancelling, Button::new(label))
            .clicked()
        {
            actions.push(if busy {
                Action::CancelPairing
            } else {
                Action::CloseJoin
            });
        }
    });
    if let Stage::Join(Joining { progress, problem }) = &pairing.stage {
        if let Some(line) = progress {
            wrapped(ui, line.as_str());
        }
        if let Some(line) = problem {
            wrapped(ui, RichText::new(line).color(BUSY));
        }
    }
}

/// How the last pairing try ended.
fn outcome_lines(ui: &mut Ui, outcome: &Outcome) {
    ui.add_space(4.0);
    for (index, line) in outcome.lines.iter().enumerate() {
        let color = match (outcome.paired, index) {
            (true, 0) => GOOD,
            (true, _) => BUSY,
            (false, _) => PROBLEM,
        };
        wrapped(ui, RichText::new(line).color(color));
    }
}

/// "Permissions", on a Mac: Accessibility.
fn permissions(ui: &mut Ui, state: PermissionState, actions: &mut Vec<Action>) {
    match state {
        PermissionState::Granted | PermissionState::NotNeeded => {
            ui.horizontal(|ui| {
                tick(ui);
                ui.label(words::ACCESSIBILITY_ALLOWED);
            });
        }
        PermissionState::Denied | PermissionState::NotDetermined => {
            ui.horizontal(|ui| {
                dot(ui, Tone::Problem);
                ui.label(words::ACCESSIBILITY_MISSING);
                if ui.button(words::ALLOW).clicked() {
                    actions.push(Action::AllowAccessibility);
                }
            });
            wrapped(ui, RichText::new(words::ACCESSIBILITY_WHY).weak());
        }
    }
}

/// "Activity": the last status lines, newest at the bottom.
fn activity(ui: &mut Ui, model: &Model, actions: &mut Vec<Action>) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ScrollArea::vertical()
            .id_salt("hop-activity")
            .max_height(170.0)
            .auto_shrink([false, true])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in model.activity.lines() {
                    wrapped(ui, RichText::new(line).small());
                }
            });
    });
    if ui.button(words::OPEN_LOG_FOLDER).clicked() {
        actions.push(Action::OpenLogFolder);
    }
}

/// "{name} ({OS}) entered the code. Pair with it?", while it waits for an
/// answer. Clicking beside it does not answer: only the two buttons do, or
/// the time running out (which counts as No). Once the time is up the
/// buttons go, and the dialog says so until the session's end arrives.
fn question_dialog(
    ctx: &egui::Context,
    pairing: &Pairing,
    now: Instant,
    actions: &mut Vec<Action>,
) {
    let Stage::Show(Showing {
        question: Some(question),
        ..
    }) = &pairing.stage
    else {
        return;
    };
    Modal::new(Id::unique("hop-pair-question")).show(ctx, |ui| {
        ui.set_max_width(380.0);
        wrapped(
            ui,
            RichText::new(words::pair_question(&question.name, question.os)).strong(),
        );
        if let Some(replaces) = &question.replaces {
            wrapped(ui, RichText::new(replaces).color(BUSY));
        }
        wrapped(ui, words::answer_only_if(&question.name));
        let left = pairing::left(question.deadline, now);
        if left.is_zero() {
            wrapped(ui, RichText::new(words::NO_ANSWER_IN_TIME).color(BUSY));
            return;
        }
        wrapped(
            ui,
            RichText::new(words::no_answer_counts_as_no(left)).weak(),
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button(RichText::new(words::YES_PAIR).strong()).clicked() {
                actions.push(Action::Answer(true));
            }
            if ui.button(words::NO).clicked() {
                actions.push(Action::Answer(false));
            }
        });
    });
}

/// "Forget {name}? You will need to pair again." Escape or a click beside
/// it cancels.
fn forget_dialog(ctx: &egui::Context, model: &Model, actions: &mut Vec<Action>) {
    let Some(peer) = model.forgetting() else {
        return;
    };
    let shown = Modal::new(Id::unique("hop-forget")).show(ctx, |ui| {
        ui.set_max_width(340.0);
        wrapped(ui, words::forget_question(&peer.name));
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button(words::FORGET).clicked() {
                actions.push(Action::Forget(peer.id.clone()));
            }
            if ui.button(words::CANCEL).clicked() {
                actions.push(Action::CancelForget);
            }
        });
    });
    if shown.should_close() {
        actions.push(Action::CancelForget);
    }
}

#[cfg(test)]
mod tests;
