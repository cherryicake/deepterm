# DeepTerm — Claude Context

## Local Reference Codebases

- `../zed/` - Zed source (use this local copy instead of GitHub)
- `~/Documents/workspace/gpui-component/` - GPUI component library reference
- `../russh/russh` - russh 0.58 local dependency

## Terminal Rendering

- Claude Code cursor uses reverse video (`Flags::INVERSE` + space char); check `INVERSE` when computing cell background in `terminal_element.rs`
- Set `TERM_PROGRAM_VERSION` for better terminal host identification

## russh 0.58 API Patterns

- `PrivateKeyWithHashAlg` is at `russh::keys::PrivateKeyWithHashAlg`
- `AuthResult` is re-exported as `russh::client::AuthResult`
- `russh::auth` module is private — do not import directly
- `best_supported_rsa_hash()` returns nested `Option`s; use `.ok().flatten().flatten()`
- `channel.request_pty()`, `request_shell()`, and `handle.disconnect()` all take `&self`

## Editor Architecture (In Progress)

- Migrating toward gpui-component's architecture: `InputState` (pure state) + `Input` (UI wrapper) + `TextElement` (custom Element)
- Avoid adding new stub modules (LSP, popovers, search, mask_pattern, number_input) — these are slated for removal

## Theme & Highlighting

- `TerminalTheme::to_highlight_theme()` generates editor highlighting from ANSI colors
- `HighlightTheme` lives in `src/highlighter/`; keep syntax theme colors synchronized with terminal theme changes

## GPUI Key Context Rules

- Global shortcuts must bind to `None` context, not a named context
- Input components should set `.key_context("Input")` for their own bindings
- Any component receiving keyboard events must call `.track_focus(&self.focus_handle)` in `render`
