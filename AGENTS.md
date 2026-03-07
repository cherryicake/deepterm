# AGENTS.md - Agentic Coding Guidelines

## Build Commands

```bash
# Build the project
cargo build

# Build in release mode
cargo build --release

# Run the application
cargo run

# Check for compilation errors without building
cargo check

# Clean build artifacts
cargo clean
```

## Lint & Format Commands

```bash
# Format code with rustfmt
cargo fmt

# Check formatting without modifying files
cargo fmt -- --check

# Run clippy lints
cargo clippy

# Run clippy with all features and fix suggestions
cargo clippy --all-features -- -D warnings

# Fix auto-fixable issues
cargo fix
```

## Test Commands

```bash
# Run all tests
cargo test

# Run a single test by name
cargo test test_name

# Run tests in a specific module
cargo test module_name::

# Run tests with output visible
cargo test -- --nocapture

# Run ignored tests
cargo test -- --ignored
```

## Code Style Guidelines

### Imports
- Group imports: std, external crates, internal modules
- Use `use crate::` for internal module imports
- Alphabetically sort within groups
- Example:
```rust
use std::collections::HashMap;

use gpui::{div, prelude::*, App, Context, Entity};

use crate::components::tab_bar::{TabBar, TabItem};
use crate::workspace::panel::{Panel, PanelEvent};
```

### Formatting
- Use 4 spaces for indentation
- Max line length: 100 characters
- Use trailing commas in multi-line structs/enums
- Always use `rustfmt` before committing

### Types & Naming
- **Types**: PascalCase (e.g., `TabInfo`, `SplitContainer`)
- **Functions/Methods**: snake_case (e.g., `handle_tab_drop_on_panel`)
- **Variables**: snake_case (e.g., `dragged_tab_id`)
- **Constants**: SCREAMING_SNAKE_CASE
- **Generic parameters**: Single uppercase letters (T, E, K, V)

### Error Handling
- Use `Result<T, E>` for recoverable errors
- Use `Option<T>` for nullable values
- Prefer `?` operator over explicit match
- Use `expect()` with descriptive messages in tests
- Use `unwrap()` sparingly, only when truly infallible

### GPUI-Specific Patterns
- Use `cx` for Context parameter names
- Use `Entity<T>` for component references
- Implement `Render` trait for UI components
- Use `cx.notify()` after state changes
- Prefer `cx.defer()` for cross-entity updates

### Comments & Documentation
- Use `///` for public API documentation
- Use `//` for inline implementation comments
- Document complex algorithms and business logic
- Keep comments concise and up-to-date

### Architecture Patterns
- Each tab owns its SplitContainer
- Workspace manages tab lifecycle
- Panel handles drag/drop events
- SplitManager for container operations
- Keep components focused and single-responsibility

## Project Structure

```
src/
├── main.rs              # Application entry point
├── components/          # Reusable UI components
│   ├── tab_bar.rs
│   ├── split_view.rs
│   └── ...
├── workspace/           # Core workspace logic
│   ├── workspace.rs     # Main workspace component
│   ├── panel.rs         # Panel component
│   ├── split_manager.rs # Container management
│   └── ...
├── layout/              # Layout management
└── panels/              # Specific panel types
```

## Dependencies

- **GPUI**: UI framework (local path dependency)
- **serde**: Serialization
- **chrono**: Date/time handling
- **dirs**: System directories

Always check Cargo.toml before adding new dependencies.

## Workflow Requirements

### Before Committing
- **Notify user** to review code changes before creating a commit
- Wait for **explicit approval** from user before running `git commit`
- Do not commit automatically without user consent

### Commit Message Format
- Use conventional commits format: `type: description`
- Types: `feat`, `fix`, `docs`, `style`, `refactor`, `test`, `chore`
- Keep description concise and clear

### Remote Repository
- Currently not bound to any remote repository
- User will provide remote URL when ready to push
