# Index

Index is a local-first desktop code search tool built with Tauri 2, Rust, SQLite, and TypeScript. Project folders and their indexed source contents stay on this machine; Index does not upload code or require an account.

## Features

- Add project folders with the native folder picker.
- Recursively index common source, markup, and configuration files. `.gitignore`, `.git/info/exclude`, hidden entries, and ignored build/dependency folders are respected.
- Store file text and metadata in a SQLite database in the operating system's application data directory.
- Reuse unchanged file records by content hash, update changed files, and remove deleted files on refresh. A recursive filesystem watcher batches changes and refreshes affected project roots; watcher errors trigger a recovery scan.
- Search files and symbols with ranked exact, prefix, subsequence, and small-edit-distance fuzzy matching. Content uses SQLite FTS5 trigram search and also includes fuzzy filename matches.
- Extract real symbols with Tree-sitter for Rust, TypeScript/TSX, JavaScript/JSX, and Lua. Symbol results show kind and declaration line. Other indexed languages remain available for file and content search without being presented as symbol-supported.
- Open results at their line in VS Code, Visual Studio, or Neovim. Automatic mode checks for VS Code first and Visual Studio second on Windows; Neovim can use a GUI frontend or Windows Terminal. Editor executables and the global shortcut can be changed in Settings; the default global shortcut is `CTRL+SHIFT+SPACE`. If an editor cannot start, Index falls back to the platform file handler.
- The global shortcut focuses the existing window; single-instance handling reuses that window when Index is launched again.

Index opens as a compact, borderless search palette. `Ctrl+K` focuses search, `Ctrl+P` opens project selection, arrow keys move through results, and Enter opens the selected file. Symbol mode recognizes common declaration forms in supported languages; it does not yet build a complete language-server symbol graph.

Index skips files larger than 1 MB and files that are not valid UTF-8. It never edits project files or sends source code to a service. Filesystem notifications are debounced; reconciliation reuses content hashes so unchanged records and symbols are not rewritten.

## Development

Prerequisites: Node.js/npm, Rust with the Windows MSVC toolchain (Windows) or a supported Linux Rust toolchain, and Tauri 2 platform prerequisites. Windows builds use the installed WebView2 runtime.

```sh
npm install
npm run tauri dev
```

Build the frontend:

```sh
npm run build
```

Build a desktop bundle:

```sh
npm run tauri build
```

On Windows, the build produces an MSI installer at `src-tauri/target/release/bundle/msi/Index_0.1.0_x64_en-US.msi` (and an NSIS installer). The app icon source is `src-tauri/icons/index.svg`; generated platform icons are stored alongside it.

Run backend tests:

```sh
cargo test --manifest-path src-tauri/Cargo.toml
```

For Linux, install Tauri's GTK/WebKitGTK and app indicator development prerequisites as described in the official Tauri 2 prerequisites documentation. Make sure `code` or `nvim` is on PATH, or set its executable path in Index Settings.

