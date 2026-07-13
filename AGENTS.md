# Repository Guidance

## TUI shortcut affordances

- Render every visible control with a keyboard shortcut in a btop-style label: style the exact shortcut grapheme separately with the accent color and bold weight, and render the rest with the normal control style. In a selected inverse-color button, the whole button may use the accent background; keep the shortcut distinct with underline, weight, or contrast instead of accent-on-accent text.
- Never highlight a letter that is not an active binding in the current view and focus. Show numeric or symbolic bindings as their actual key; use `↵` for Enter and `←` for back navigation.
- Keep the whole label clickable. Compute hitboxes from Unicode display width and keep their geometry stable across active, inactive, focused, compact, and light/dark states.
- Text-entry focus must consume printable keys before global shortcuts.
- New shortcut-labelled controls require render, keyboard, and mouse tests, including compact terminal coverage.
