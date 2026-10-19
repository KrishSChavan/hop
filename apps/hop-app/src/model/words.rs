//! Everything the window says, in one place. The sentences in quotes in
//! docs/M6-APP.md ("The window") are normative, because docs/INSTALL.md
//! quotes them; the tests below pin them.

use std::net::IpAddr;
use std::time::Duration;

use hop_agent::settings::opposite;
use hop_agent::{CmdCtrl, Os, Side};

/// The window's title, and the name in its header.
pub const HOP: &str = "Hop";

/// The sections, top to bottom: starting and stopping sharing.
pub const SHARING: &str = "Sharing";
/// The paired computers, where they sit, their addresses.
pub const YOUR_OTHER_COMPUTER: &str = "Your other computer";
/// What Cmd and Ctrl do with the other computer's keyboard here.
pub const SHORTCUTS: &str = "Shortcuts";
/// Pairing.
pub const PAIRING: &str = "Pair";
/// The Accessibility permission (Mac only).
pub const PERMISSIONS: &str = "Permissions";
/// The last status lines.
pub const ACTIVITY: &str = "Activity";

/// "Not sharing."
pub const NOT_SHARING: &str = "Not sharing.";
/// The status line while the agent starts (or starts again after a change).
pub const STARTING: &str = "Starting…";
/// The status line between Stop sharing and the agent's last word.
pub const STOPPING: &str = "Stopping…";
/// The sharing button.
pub const START_SHARING: &str = "Start sharing";
/// The sharing button while sharing.
pub const STOP_SHARING: &str = "Stop sharing";
/// Why the sharing button is greyed out.
pub const PAIR_FIRST: &str = "Pair with your other computer first.";
/// Why the sharing button is greyed out when the settings could not be read.
pub const SETTINGS_FIRST: &str = "Hop cannot share until it can read its settings (see above).";
/// Shown on Windows at all times: the firewall's first question.
pub const FIREWALL: &str =
    "If Windows asks whether to let Hop use networks, choose Private networks and click Allow.";

/// "Your other computer" when nothing is paired.
pub const NOT_PAIRED_YET: &str = "Not paired yet. Use Pair below.";
/// "Your other computer" when the settings could not be read.
pub const PEERS_UNKNOWN: &str =
    "Hop cannot show your other computer until it can read its settings (see above).";
/// The label above the four side buttons.
pub const WHERE_IS_IT: &str = "Where is it?";
/// The address field's label.
pub const ADDRESS: &str = "Address";
/// What the address field is for.
pub const ADDRESS_NOTE: &str = "only if it is not found automatically";
/// The address field's button.
pub const SAVE_ADDRESS: &str = "Save address";

/// On a Mac, for keys from the PC's keyboard: every key arrives as itself.
pub const MAC_KEEP_KEYS: &str = "Keep keys as they are: the Windows key is Cmd";
/// On a Mac, for keys from the PC's keyboard: Ctrl and Cmd trade places.
pub const MAC_SWAP_KEYS: &str = "Swap Ctrl and Cmd: Ctrl+C copies, as on a PC";
/// On the PC, for keys from the Mac's keyboard: every key arrives as itself.
pub const PC_KEEP_KEYS: &str = "Keep keys as they are: Ctrl+C copies, Cmd is the Windows key";
/// On the PC, for keys from the Mac's keyboard: Cmd and Ctrl trade places.
pub const PC_SWAP_KEYS: &str = "Swap Cmd and Ctrl: Cmd+C copies, as on a Mac";
/// "Shortcuts" when the settings could not be read.
pub const SHORTCUTS_UNKNOWN: &str =
    "Hop cannot show this choice until it can read its settings (see above).";
/// Greyed out in an empty address field.
pub const ADDRESS_HINT: &str = "e.g. 192.168.1.23";
/// Greyed out in an empty Code field.
pub const CODE_HINT: &str = "6 digits";
/// Opens the forget dialog.
pub const FORGET_ELLIPSIS: &str = "Forget…";
/// The forget dialog's yes.
pub const FORGET: &str = "Forget";
/// The forget dialog's no, and the button that stops pairing.
pub const CANCEL: &str = "Cancel";

/// One way to pair: this computer shows a code.
pub const SHOW_CODE: &str = "Show a pairing code";
/// The other way: this computer types the code the other one shows.
pub const ENTER_CODE: &str = "Enter a code";
/// Under the code.
pub const CODE_HELP: &str = "On the other computer, open Hop, click Enter a code and type this \
                             code. It works once, for 2 minutes. Never show it in a screen share.";
/// While the listener starts, before the code is known.
pub const GETTING_CODE: &str = "Getting a code…";
/// The pairing question's yes.
pub const YES_PAIR: &str = "Yes, pair";
/// The pairing question's no.
pub const NO: &str = "No";
/// The "Enter a code" form's code field.
pub const CODE: &str = "Code";
/// Above the form's address field.
pub const NOT_FOUND: &str = "If the other computer is not found";
/// The form's button.
pub const PAIR: &str = "Pair";
/// Pairing progress on the computer typing the code: looking for the other.
pub const LOOKING: &str = "Looking for the computer showing the code…";
/// Pairing progress: found it, connecting.
pub const CONNECTING: &str = "Connecting…";
/// After the user cancelled.
pub const PAIRING_CANCELLED: &str = "Pairing cancelled.";
/// Between Cancel and the session's end.
pub const CANCELLING: &str = "Cancelling…";

/// The Accessibility permission, on a Mac, when granted.
pub const ACCESSIBILITY_ALLOWED: &str = "Accessibility: Allowed";
/// The Accessibility permission when not granted (yet).
pub const ACCESSIBILITY_MISSING: &str = "Accessibility: Not allowed yet";
/// Shows macOS's prompt and opens the Accessibility settings.
pub const ALLOW: &str = "Allow…";
/// Why Hop asks for Accessibility.
pub const ACCESSIBILITY_WHY: &str =
    "Hop needs Accessibility to share this Mac's keyboard and mouse.";

/// Under Activity.
pub const OPEN_LOG_FOLDER: &str = "Open log folder";
/// The settings could not be read.
pub const TRY_AGAIN: &str = "Try again";

/// How long a code works; hop-net's `PairingTiming` default, which the
/// sentences above promise.
pub const CODE_LIFETIME: Duration = Duration::from_secs(120);

/// `text` as a sentence: with a full stop at the end unless it already
/// ends in one (or in another closing mark).
pub fn sentence(text: &str) -> String {
    let text = text.trim_end();
    if text.ends_with(['.', '!', '?', '…']) {
        text.to_owned()
    } else {
        format!("{text}.")
    }
}

/// "This computer: {name} ({OS})".
pub fn this_computer(name: &str, os: Os) -> String {
    format!("This computer: {name} ({os})")
}

/// "{name} ({OS})".
pub fn computer(name: &str, os: Os) -> String {
    format!("{name} ({os})")
}

/// "Looking for {peer}…".
pub fn looking_for(peer: &str) -> String {
    format!("Looking for {peer}…")
}

/// "Connected to {peer}."
pub fn connected_to(peer: &str) -> String {
    format!("Connected to {peer}.")
}

/// "Using {peer}: this keyboard and mouse control it now."
pub fn using(peer: &str) -> String {
    format!("Using {peer}: this keyboard and mouse control it now.")
}

/// "{peer} is using this computer."
pub fn used_by(peer: &str) -> String {
    format!("{peer} is using this computer.")
}

/// "Stopped: {reason}", with the reason in full.
pub fn stopped(reason: &str) -> String {
    format!("Stopped: {}", sentence(reason))
}

/// The button label for a side, as the user picks it.
pub fn side_label(side: Side) -> &'static str {
    match side {
        Side::Left => "Left",
        Side::Right => "Right",
        Side::Above => "Above",
        Side::Below => "Below",
    }
}

/// The edge of this computer's screens that leads to a computer on `side`.
pub fn edge(side: Side) -> &'static str {
    match side {
        Side::Left => "left",
        Side::Right => "right",
        Side::Above => "top",
        Side::Below => "bottom",
    }
}

/// Where a computer on `side` sits, for sentences.
pub fn placement(side: Side) -> &'static str {
    match side {
        Side::Left => "to the left of",
        Side::Right => "to the right of",
        Side::Above => "above",
        Side::Below => "below",
    }
}

/// "Push the cursor past the {edge} edge of this screen to move to {peer}."
pub fn push_past(side: Side, peer: &str) -> String {
    format!(
        "Push the cursor past the {} edge of this screen to move to {peer}.",
        edge(side)
    )
}

/// "On {name}, choose {opposite} for this computer."
pub fn choose_there(name: &str, side: Side) -> String {
    format!(
        "On {name}, choose {} for this computer.",
        side_label(opposite(side))
    )
}

/// Above the Shortcuts choice: whose keys it is about. The computer the
/// keys arrive on applies the choice, so here it is about the other
/// computer's keyboard, named when it is known.
pub fn with_keyboard(peer: Option<&str>) -> String {
    match peer {
        Some(name) => format!("With {name}'s keyboard on this computer:"),
        None => "With the other computer's keyboard on this computer:".to_owned(),
    }
}

/// The Shortcuts choice on `os`, keeping keys as they are first: on a Mac
/// it is about keys from a PC's keyboard, elsewhere about keys from a Mac's.
pub fn shortcut_choices(os: Os) -> [(CmdCtrl, &'static str); 2] {
    match os {
        Os::MacOs => [
            (CmdCtrl::Keep, MAC_KEEP_KEYS),
            (CmdCtrl::Swap, MAC_SWAP_KEYS),
        ],
        Os::Windows | Os::Linux => [(CmdCtrl::Keep, PC_KEEP_KEYS), (CmdCtrl::Swap, PC_SWAP_KEYS)],
    }
}

/// Under the Shortcuts choice: this computer's own keyboard is the other
/// computer's choice.
pub fn this_keyboard_there(peer: Option<&str>) -> String {
    format!(
        "The choice for this computer's keyboard is in Hop on {}.",
        peer.unwrap_or("the other computer")
    )
}

/// How many names the Activity line about files that arrived lists; the
/// rest are counted.
pub const NAMES_LISTED: usize = 10;

/// The Activity line naming the files that just arrived from `peer`: "Files
/// from Mac: notes.txt, photo.jpg", the first [`NAMES_LISTED`] by name and
/// the rest counted. It is shown in the window and never logged.
pub fn files_from(peer: &str, names: &[String]) -> String {
    let what = if names.len() == 1 { "File" } else { "Files" };
    let listed: Vec<String> = names
        .iter()
        .take(NAMES_LISTED)
        .map(|name| printable(name))
        .collect();
    let mut line = format!("{what} from {peer}: {}", listed.join(", "));
    let more = names.len().saturating_sub(NAMES_LISTED);
    if more > 0 {
        line.push_str(&format!(" and {more} more"));
    }
    line
}

/// `name` with anything that could garble a line shown as "\u{FFFD}":
/// control characters, and the marks that reorder text. Hop cleans names
/// before it saves a file; this is the window's own guard.
fn printable(name: &str) -> String {
    name.chars()
        .map(|c| {
            let reorders = matches!(
                c,
                '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
            );
            if c.is_control() || reorders {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect()
}

/// "Forget {name}? You will need to pair again."
pub fn forget_question(name: &str) -> String {
    format!("Forget {name}? You will need to pair again.")
}

/// "Someone entered a wrong code (1 of 3 tries used)."
pub fn wrong_code(failures: u32) -> String {
    format!("Someone entered a wrong code ({failures} of 3 tries used).")
}

/// "{name} ({OS}) entered the code. Pair with it?"
pub fn pair_question(name: &str, os: Os) -> String {
    format!("{name} ({os}) entered the code. Pair with it?")
}

/// "Answer Yes only if {name} says it is waiting for you."
pub fn answer_only_if(name: &str) -> String {
    format!("Answer Yes only if {name} says it is waiting for you.")
}

/// Under the pairing question once its time is up, in place of the buttons:
/// it counted as No, and the session's end is on its way.
pub const NO_ANSWER_IN_TIME: &str = "No answer in time.";

/// Under the pairing question, while it waits.
pub fn no_answer_counts_as_no(left: Duration) -> String {
    format!(
        "No answer before the time runs out counts as No ({} left).",
        clock(left)
    )
}

/// After the question was answered yes, until pairing ends.
pub fn pairing_with(name: &str) -> String {
    format!("Pairing with {name}…")
}

/// "Now click Yes, pair on {host}."
pub fn click_yes_on(host: &str) -> String {
    format!("Now click Yes, pair on {host}.")
}

/// The time left on the code.
pub fn time_left(left: Duration) -> String {
    format!("Time left: {}", clock(left))
}

/// `1:47`: minutes and seconds, rounded up, so the clock shows 0:00 only
/// once the time is really up.
pub fn clock(left: Duration) -> String {
    let secs = left.as_secs() + u64::from(left.subsec_nanos() > 0);
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// Shown as a code is shown (never the code itself: Activity lines may be
/// read over a shoulder or in a screen share).
pub const SHOWING_CODE: &str = "Showing a pairing code. It works once, for 2 minutes.";

/// When this computer could not be announced on the local network.
pub fn not_announced(reason: &str) -> String {
    format!(
        "Could not announce this computer on the local network ({reason}). On the other \
         computer, type this computer's IP address in Address, under \"{NOT_FOUND}\"."
    )
}

/// When the other computer entered the right code (the question follows).
pub fn entered_the_code(name: &str, os: Os) -> String {
    format!("{name} ({os}) entered the code.")
}

/// When no computer showing a code was found: points to the Address field.
pub const NO_HOST: &str = "No computer showing a pairing code was found. Check that the other \
                           computer shows one. If it does, type its address in Address, under \
                           \"If the other computer is not found\", and click Pair again.";

/// When several computers show a code: lists them and points to the
/// Address field. Hop never picks one, because it could be an impostor.
pub fn several_hosts(hosts: &[(String, Vec<IpAddr>)]) -> String {
    let found: Vec<String> = hosts
        .iter()
        .map(|(name, ips)| {
            let ips: Vec<String> = ips.iter().map(ToString::to_string).collect();
            if ips.is_empty() {
                name.clone()
            } else {
                format!("{name} ({})", ips.join(", "))
            }
        })
        .collect();
    format!(
        "More than one computer is showing a pairing code: {}. Type the address of the one you \
         mean in Address, under \"{NOT_FOUND}\", and click Pair again.",
        found.join("; ")
    )
}

/// "Paired with {name}. It sits {placement} this screen; change that under
/// Your other computer."
pub fn paired(name: &str, side: Side) -> String {
    format!(
        "Paired with {name}. It sits {} this screen; change that under Your other computer.",
        placement(side)
    )
}

/// When the new pairing replaced one with another key.
pub fn replaced_old_key(old: &str) -> String {
    format!(
        "It replaces the pairing with {old}, whose old key can no longer connect to this \
         computer."
    )
}

/// When this computer saved the pairing but the other may not have heard.
pub fn however(reason: &str) -> String {
    format!("However: {}", sentence(reason))
}

/// "Not paired: {reason}."
pub fn not_paired(reason: &str) -> String {
    format!("Not paired: {}", sentence(reason))
}

/// When the settings could not be read.
pub fn settings_failed(error: &str) -> String {
    format!("Hop could not read its settings: {}", sentence(error))
}

/// When a change could not be saved.
pub fn not_saved(error: &str) -> String {
    format!("Not saved: {}", sentence(error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sentences_docs_install_md_quotes_are_unchanged() {
        assert_eq!(NOT_SHARING, "Not sharing.");
        assert_eq!(looking_for("Windows PC"), "Looking for Windows PC…");
        assert_eq!(connected_to("Windows PC"), "Connected to Windows PC.");
        assert_eq!(
            using("Windows PC"),
            "Using Windows PC: this keyboard and mouse control it now."
        );
        assert_eq!(used_by("Mac"), "Mac is using this computer.");
        assert_eq!(
            push_past(Side::Right, "Windows PC"),
            "Push the cursor past the right edge of this screen to move to Windows PC."
        );
        assert_eq!(PAIR_FIRST, "Pair with your other computer first.");
        assert_eq!(
            FIREWALL,
            "If Windows asks whether to let Hop use networks, choose Private networks and click \
             Allow."
        );
        assert_eq!(
            pair_question("Mac", Os::MacOs),
            "Mac (macOS) entered the code. Pair with it?"
        );
        assert_eq!(LOOKING, "Looking for the computer showing the code…");
        assert_eq!(CONNECTING, "Connecting…");
        assert_eq!(
            click_yes_on("Windows PC"),
            "Now click Yes, pair on Windows PC."
        );
        assert_eq!(ACCESSIBILITY_ALLOWED, "Accessibility: Allowed");
        assert_eq!(ACCESSIBILITY_MISSING, "Accessibility: Not allowed yet");
        assert_eq!(
            CODE_HELP,
            "On the other computer, open Hop, click Enter a code and type this code. It works \
             once, for 2 minutes. Never show it in a screen share."
        );
        assert_eq!(
            wrong_code(1),
            "Someone entered a wrong code (1 of 3 tries used)."
        );
        assert_eq!(
            answer_only_if("Mac"),
            "Answer Yes only if Mac says it is waiting for you."
        );
        assert_eq!(
            forget_question("Windows PC"),
            "Forget Windows PC? You will need to pair again."
        );
        assert_eq!(NOT_PAIRED_YET, "Not paired yet. Use Pair below.");
        assert_eq!(
            this_computer("Mac", Os::MacOs),
            "This computer: Mac (macOS)"
        );
        assert_eq!(SHORTCUTS, "Shortcuts");
        assert_eq!(
            PC_KEEP_KEYS,
            "Keep keys as they are: Ctrl+C copies, Cmd is the Windows key"
        );
        assert_eq!(PC_SWAP_KEYS, "Swap Cmd and Ctrl: Cmd+C copies, as on a Mac");
        assert_eq!(
            MAC_KEEP_KEYS,
            "Keep keys as they are: the Windows key is Cmd"
        );
        assert_eq!(
            MAC_SWAP_KEYS,
            "Swap Ctrl and Cmd: Ctrl+C copies, as on a PC"
        );
    }

    #[test]
    fn each_computer_words_the_shortcuts_for_the_other_ones_keyboard() {
        assert_eq!(
            shortcut_choices(Os::MacOs),
            [
                (
                    CmdCtrl::Keep,
                    "Keep keys as they are: the Windows key is Cmd"
                ),
                (
                    CmdCtrl::Swap,
                    "Swap Ctrl and Cmd: Ctrl+C copies, as on a PC"
                ),
            ]
        );
        assert_eq!(
            shortcut_choices(Os::Windows),
            [
                (
                    CmdCtrl::Keep,
                    "Keep keys as they are: Ctrl+C copies, Cmd is the Windows key"
                ),
                (
                    CmdCtrl::Swap,
                    "Swap Cmd and Ctrl: Cmd+C copies, as on a Mac"
                ),
            ]
        );
        assert_eq!(shortcut_choices(Os::Linux), shortcut_choices(Os::Windows));
        assert_eq!(
            with_keyboard(Some("Windows PC")),
            "With Windows PC's keyboard on this computer:"
        );
        assert_eq!(
            with_keyboard(None),
            "With the other computer's keyboard on this computer:"
        );
        assert_eq!(
            this_keyboard_there(Some("Mac")),
            "The choice for this computer's keyboard is in Hop on Mac."
        );
        assert_eq!(
            this_keyboard_there(None),
            "The choice for this computer's keyboard is in Hop on the other computer."
        );
    }

    #[test]
    fn files_that_arrived_are_named_and_the_rest_counted() {
        let names = |n: usize| -> Vec<String> { (1..=n).map(|i| format!("f{i}.txt")).collect() };
        assert_eq!(
            files_from("Mac", &["notes.txt".to_owned()]),
            "File from Mac: notes.txt"
        );
        assert_eq!(
            files_from("Mac", &["notes.txt".to_owned(), "photo (2).jpg".to_owned()]),
            "Files from Mac: notes.txt, photo (2).jpg"
        );
        assert_eq!(
            files_from("Mac", &names(NAMES_LISTED)),
            "Files from Mac: f1.txt, f2.txt, f3.txt, f4.txt, f5.txt, f6.txt, f7.txt, f8.txt, \
             f9.txt, f10.txt"
        );
        assert_eq!(
            files_from("Mac", &names(1000)),
            "Files from Mac: f1.txt, f2.txt, f3.txt, f4.txt, f5.txt, f6.txt, f7.txt, f8.txt, \
             f9.txt, f10.txt and 990 more"
        );
        // A name cannot break the line or turn the text around.
        assert_eq!(
            files_from("Mac", &["a\nb\u{202E}txt.exe".to_owned()]),
            "File from Mac: a\u{FFFD}b\u{FFFD}txt.exe"
        );
    }

    #[test]
    fn each_side_has_its_button_edge_and_placement() {
        let words: Vec<_> = [Side::Left, Side::Right, Side::Above, Side::Below]
            .into_iter()
            .map(|side| (side_label(side), edge(side), placement(side)))
            .collect();
        assert_eq!(
            words,
            [
                ("Left", "left", "to the left of"),
                ("Right", "right", "to the right of"),
                ("Above", "top", "above"),
                ("Below", "bottom", "below"),
            ]
        );
    }

    #[test]
    fn the_tip_names_the_opposite_side() {
        assert_eq!(
            choose_there("Windows PC", Side::Right),
            "On Windows PC, choose Left for this computer."
        );
        assert_eq!(
            choose_there("Mac", Side::Above),
            "On Mac, choose Below for this computer."
        );
    }

    #[test]
    fn results_read_as_the_spec_says() {
        assert_eq!(
            paired("Windows PC", Side::Right),
            "Paired with Windows PC. It sits to the right of this screen; change that under Your \
             other computer."
        );
        assert_eq!(
            paired("Mac", Side::Below),
            "Paired with Mac. It sits below this screen; change that under Your other computer."
        );
        assert_eq!(
            not_paired("nothing was saved on either computer"),
            "Not paired: nothing was saved on either computer."
        );
        assert_eq!(
            stopped("UDP port 47800 is already in use"),
            "Stopped: UDP port 47800 is already in use."
        );
    }

    #[test]
    fn a_sentence_gets_one_full_stop() {
        assert_eq!(sentence("cancelled"), "cancelled.");
        assert_eq!(sentence("done."), "done.");
        assert_eq!(sentence("really?  "), "really?");
        assert_eq!(sentence("(the log says where)"), "(the log says where).");
    }

    #[test]
    fn the_clock_rounds_up_so_zero_means_the_time_is_up() {
        assert_eq!(clock(Duration::from_secs(120)), "2:00");
        assert_eq!(clock(Duration::from_millis(106_200)), "1:47");
        assert_eq!(clock(Duration::from_millis(1)), "0:01");
        assert_eq!(clock(Duration::ZERO), "0:00");
        assert_eq!(time_left(Duration::from_secs(9)), "Time left: 0:09");
    }

    #[test]
    fn several_hosts_are_listed_with_their_addresses() {
        let hosts = vec![
            ("PC".to_owned(), vec!["192.168.1.20".parse().expect("ip")]),
            (
                "Laptop".to_owned(),
                vec![
                    "192.168.1.21".parse().expect("ip"),
                    "10.0.0.5".parse().expect("ip"),
                ],
            ),
            ("Quiet".to_owned(), Vec::new()),
        ];
        let text = several_hosts(&hosts);
        assert!(
            text.starts_with(
                "More than one computer is showing a pairing code: PC (192.168.1.20); Laptop \
                 (192.168.1.21, 10.0.0.5); Quiet."
            ),
            "{text}"
        );
        assert!(text.contains("in Address"), "{text}");
    }
}
