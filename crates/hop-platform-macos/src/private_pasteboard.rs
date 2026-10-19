//! Private pasteboards for tests. Each is made with
//! `pasteboardWithUniqueName` and released when dropped, so no test ever
//! reads or changes the clipboard or the drag pasteboard that apps use.

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{Message, msg_send};
use objc2_app_kit::{NSPasteboard, NSPasteboardWriting};
use objc2_foundation::{NSArray, NSString};

/// A pasteboard of its own, released when dropped.
pub(crate) struct PrivatePasteboard {
    pub(crate) pasteboard: Retained<NSPasteboard>,
    name: String,
}

impl PrivatePasteboard {
    pub(crate) fn new() -> Self {
        let pasteboard = NSPasteboard::pasteboardWithUniqueName();
        let name = pasteboard.name().to_string();
        assert!(!name.is_empty());
        PrivatePasteboard { pasteboard, name }
    }

    /// Finds this pasteboard by name, on any thread, as the helpers find
    /// theirs for each call.
    pub(crate) fn opener(
        &self,
    ) -> impl Fn() -> Option<Retained<NSPasteboard>> + Send + Sync + 'static {
        let name = self.name.clone();
        move || Some(NSPasteboard::pasteboardWithName(&NSString::from_str(&name)))
    }

    /// Replaces what the pasteboard holds with `objects`, as an app that
    /// copies or starts a drag does.
    pub(crate) fn write<T: NSPasteboardWriting + Message + 'static>(
        &self,
        objects: Vec<Retained<T>>,
    ) {
        self.pasteboard.clearContents();
        let objects: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = objects
            .into_iter()
            .map(ProtocolObject::from_retained)
            .collect();
        assert!(
            self.pasteboard
                .writeObjects(&NSArray::from_retained_slice(&objects))
        );
    }
}

impl Drop for PrivatePasteboard {
    fn drop(&mut self) {
        // SAFETY: `releaseGlobally` takes no arguments and returns nothing;
        // it frees the pasteboard server's copy, which only this test used.
        unsafe {
            let _: () = msg_send![&*self.pasteboard, releaseGlobally];
        }
    }
}
