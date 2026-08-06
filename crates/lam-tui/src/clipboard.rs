//! System clipboard access for copied text selections.
//!
//! The clipboard is touched only from the main event loop, never from the
//! app model, so unit tests exercise selection-to-text conversion without
//! reaching for the system clipboard (which may not exist in CI).

use std::io;

/// Copies `text` into the system clipboard. macOS uses the pasteboard,
/// Linux uses Wayland or X11 depending on the active session.
pub(crate) fn copy_to_clipboard(text: &str) -> io::Result<()> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|error| io::Error::other(error.to_string()))?;
    clipboard
        .set_text(text.to_owned())
        .map_err(|error| io::Error::other(error.to_string()))
}
