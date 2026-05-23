# TODO

## Quick Note rewrite

Rewrite Quick Note to use a standard native Win32 `EDIT` control instead of custom GDI text input.

Requirements:
- Keep the small topmost Quick Note popup window.
- Use a standard multiline `EDIT` control for text entry.
- Preserve Unicode input, keyboard navigation, clipboard shortcuts, caret handling, and repaint via the native control.
- `Enter` saves the note and closes/hides the popup.
- `Shift+Enter` inserts a new line.
- `Escape` cancels/closes the popup without saving.
- Save notes to `~/.config/mhd/notes/YYYY-MM-DD.md` with timestamped markdown entries.
- Keep blackbox integration if the `blackbox` feature is enabled.
- Avoid custom text buffering/caret/painting for the input field.
- No new dependencies.
