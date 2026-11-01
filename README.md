# term.hx

![Steel](assets/steel.svg) ![GitHub License](https://img.shields.io/github/license/gravures/term.hx)

Terminal integration Steel plugin for [Helix](https://github.com/helix-editor/helix). Runs programs (in a real PTY with VT emulation) directly inside the split you launched it from.

## Requirements

- An Helix [Steel-enabled build](https://github.com/mattwparas/helix/tree/steel-event-system).
- The [Rust toolchain](https://www.rust-lang.org/tools/install) installed.
- Ensure you have the **Steel** *interpreter* and *dylib*, the Steel package manager **forge** installed.

## Installation

Install as a Steel package with **forge**:
```sh
forge pkg install --git https://github.com/gravures/term.hx
```

This will build and install the plugin into `~/.local/share/steel/cogs/`.
Fully quit and restart Helix so the native library loads.

## Usage

### Command

```
:open-tool <program> [args...]
```

Opens `program` in a PTY overlay anchored to the current split. Every keystroke goes to the program until you press the **release chord** (default `ctrl-]`) or move focus to another split. The overlay stays painted and the child keeps running; press the chord again to re-acquire.

### Examples

```scheme
(open-tool "lazygit")
(open-tool "ipython")
(open-tool "/absolute/path/to/binary" "arg1")
```

### Functions

| Function | Description |
|---|---|
| `(open-tool program . args)` | Open a program in a PTY overlay |
| `(close-tool)` | Close the running tool and kill the child |
| `*open-tool-release-chord*` | Configurable release chord (default `'(ctrl #\])`) |

### Keymap

Bind `:open-tool` to a key, e.g. in your `init.scm`:

```scheme
(keymap (global)
        (normal (space (g ":open-tool lazyit"))))
```

While the tool has focus, `ctrl-]` toggles key ownership between the tool and Helix. `ctrl-]` again gives keys back to the tool.

## Configuration

### Release chord

Change the chord that returns keys to Helix (set in `init.scm` **after** requiring `open-tool.scm`):

```scheme
;; Use alt-w instead of ctrl-]
(set! *open-tool-release-chord* '(alt #\w))
```

Format: `(modifier key)` where modifier is `ctrl`, `alt`, or `shift`. Key is a character: `#\]`, `#\w`, `#\Space`, etc.

### Overlay behaviour (via split-overlay.scm)

`open-tool` uses `split-overlay.scm` for placement and lifecycle. The overlay:

- Stays anchored to the split it was opened from
- Follows that split on resize/layout changes
- Closes automatically when the anchor document leaves the screen (closed or swapped out of every view)
- Closes when the child process exits

## Architecture

```
term.hx
├── term.scm           # FFI bindings to libsteel_wezterm (wezterm-term VT emulator + portable-pty)
├── split-overlay.scm  # Split-anchored overlay infrastructure (component lifecycle, region calc, render helpers)
├── open-tool.scm      # High-level tool: spawns PTY, wires pump/render/input, mode indicator
└── Cargo.toml         # Builds libsteel_wezterm (cdylib) with wezterm-term, steel-core, portable-pty
```

### Data flow

1. `open-tool` resolves the program, spawns a PTY via `create-native-pty-system-with-args-and-cwd!`
2. Creates a `virtual-terminal` (VT emulator) bound to the PTY
3. Opens a `split-overlay` component with:
   - `render-fn` → paints VT cells via `frame-set-string!` using theme-aware colour translation
   - `event-fn` → forwards keys to PTY, handles release chord, auto-release on focus loss
   - `cursor-fn` → reports VT cursor position while tool has focus
4. Async pump (`%pump`) reads PTY output and feeds the VT emulator
5. On child exit → kill-switch arms → overlay closes synchronously on next event

### Native library (steel-wezterm)

Built from `wezterm-term` (fork branch `mwp-public-scroll`), exposing:

- PTY spawn (`portable-pty`)
- VT emulator (`wezterm-term`): parser, grid, cursor, scroll, colour attributes
- Cell iteration API for zero-copy paint loop
- Resize propagation to both VT grid and PTY

### Overlay lifecycle

```
open-tool → split-overlay-open! → push-component!
     │                                    │
     ▼                                    ▼
pump starts                       post-command hook
     │              checks anchor doc in view
     ▼                                    │
child exits ──────────────► kill-switch ──► split-overlay-close!
     │                                    │
     ▼                                    ▼
pop-last-component-by-name! ← on-close hook (kills PTY, restores mode word)
```

## Building from source (development)

```sh
# In the term.hx checkout
cargo steel-lib         # builds and installs to $STEEL_HOME/native/
```

For iterative development, symlink the cog into `~/.local/share/steel/cogs/term/` so edits apply without reinstalling.

## Troubleshooting

### Plugin fails to load / update doesn't take effect

The native library must be built against the same Steel revision as Helix and installed into the `STEEL_HOME` used by Helix. Run `cargo steel-lib` and verify it copied `libsteel_wezterm` into `$STEEL_HOME/native`. Fully quit and restart Helix after installing an update — each Helix process keeps its loaded library in memory.

### Command not found

`open-tool` checks `executable-exists?` before spawning. If the program isn't on `$PATH` (or the path doesn't exist), an error appears in the status line.

### Unforwardable keys

Keys without a printable representation are swallow by design.

## Acknowledgments

- [steel-pty](https://github.com/mattwparas/steel-pty) original rust dylib
- [steel-pty fork](https://github.com/bojohnson5/steel-pty.git) fork term.hx's dylib was based on
- [@mattwparas](https://github.com/mattwparas/) for steel-pty, the Steel plugin system, and the wezterm-term fork
- The [wezterm](https://github.com/wez/wezterm) project for the VT emulator
- [portable-pty](https://github.com/wez/portable-pty) for cross-platform PTY handling

## Contributing

Contributors are always welcome. Feel free to grab an [issue](https://github.com/gravures/term.hx/issues) to work on or make a suggested improvement. If you wish to contribute, please read the [Contribution Guide](CONTRIBUTING.md) and [Code of Conduct](CODE_OF_CONDUCT.md). <!-- rumdl-disable-line MD013 -->


## License

Use of this repository is authorized under the [GPL-3.0](https://github.com/gravures/term.hx/LICENSE).
