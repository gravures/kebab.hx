# Kebab

![Steel](assets/steel.svg) ![GitHub License](https://img.shields.io/github/license/gravures/kebab.hx)

Quality of life plugins collection for [Helix](https://github.com/helix-editor/helix).

## Requirements

- An *Helix Steel-enabled* build:
  * [Matthew Paras build](https://github.com/mattwparas/helix/tree/steel-event-system)
  * [Helix-Unstable build](https://github.com/gravures/helix/tree/unstable-steel)
- The [Rust toolchain](https://www.rust-lang.org/tools/install) installed.
- Ensure you have the **Steel** *interpreter* and *dylib*, the Steel package manager **forge** installed.

## Installation

Install as a Steel package with **forge**:
```sh
forge pkg install --git https://github.com/gravures/kebab.hx
```

This will build and install the plugins and modules into `~/.local/share/steel/cogs/kebab/`. Fully quit and restart Helix so the native library loads.

## Plugins

### Tool Plugin

| Function | Description |
|---|---|
| `open-tool <program> [args...]` | Open a program in a PTY overlay |
| `close-tool` | Close the running tool and kill the child |
| `open-tool-release-chord` | Configurable release chord (default `'(ctrl #\])`) |

Opens `program` in a PTY overlay anchored to the current split. Every keystroke goes to the program until you press the **release chord** (default `ctrl-]`) or move focus to another split. The overlay stays painted and the child keeps running; press the chord again to re-acquire.
Bind `:open-tool` to a key, e.g. in your `init.scm`:

```scheme
(keymap (global)
        (normal (space
          (g ":open-tool lazyit")
          (p ":open-tool ipyton")
          (t ":open-tool zsh")
          )))
```

While the tool has focus, `ctrl-]` toggles key ownership between the tool and Helix. `ctrl-]` again gives keys back to the tool. Change the chord that returns keys to Helix (set in `init.scm` **after** requiring `open-tool.scm`):

```scheme
;; Use alt-w instead of ctrl-]
(set! *open-tool-release-chord* '(alt #\w))
```

Format: `(modifier key)` where modifier is `ctrl`, `alt`, or `shift`. Key is a character: `#\]`, `#\w`, `#\Space`, etc.

> NOTE: Unforwardable keys
> Keys without a printable representation are swallow by design.

### Blame Plugin

| Function | Description |
|---|---|
| `git-blame` | Echo the blame of the current line in the status line |
| `git-blame-hint` | Show the blame of the current line as an inlay hint |

The plugin runs `git blame -L <line>,<line>` on the focused file and
formats the result as `<short sha> <author> <date>`.

`git-blame` writes that text in the status line. `git-blame-hint` writes it at the end of the current line, as an inlay hint. Running it again replaces the previous hint: only one hint is kept at a time.

The current file must be tracked by git. When git fails, the command does nothing: no error, no status line.

Inlay hints need to be enabled, else Helix resets them on every refresh. The plugin sets `lsp.display-inlay-hints` to `#t` when loaded.

Bind the commands to keys, e.g. in your `init.scm`:

```scheme
(keymap (global)
        (normal (space
          (b ":git-blame")
          (B ":git-blame-hint")
          )))
```

> NOTE: The author name comes from the text between parentheses in the git output. An author name containing `)` is parsed wrongly.

### Markdown Plugin

| Function | Description |
|---|---|
| `md-preview-open` | Open a markdown preview over the current split |
| `md-preview-close` | Close the markdown preview |
| `md-preview-toggle` | Open the preview, or close it when it is already open |

The preview renders the markdown of the current buffer as a floating box, centred over the current split. It follows the split on resize and layout changes, and closes when the source document leaves the screen.

The preview re-reads the buffer on each frame: an edit in the source shows up without reopening the preview. Parsing happens once per source change, not once per frame.

Press **Esc** to close the preview. The command line also closes it.

Bind the toggle to a key, e.g. in your `init.scm`:

```scheme
(keymap (global)
        (normal (space
          (m ":md-preview-toggle")
          )))
```

## Libraries

### Overlay Module

The **tool** and **markdown** plugins use the `split-overlay.scm` module for placement and lifecycle. The overlay:

- Stays anchored to the split it was opened from
- Follows that split on resize/layout changes
- Closes automatically when the anchor document leaves the screen (closed or swapped out of every view)
- Closes when the child process exits

### Native Rust Library (steel-term)

Built from `wezterm-term`  exposing:

- PTY spawn (`portable-pty`)
- VT emulator (`wezterm-term`): parser, grid, cursor, scroll, colour attributes
- Cell iteration API for zero-copy paint loop
- Resize propagation to both VT grid and PTY

## Building from source (development)

```sh
git clone https://github.com/gravures/kebab.hx.git
forge install ./kebab.hx
```

For iterative development, symlink the cog into `~/.local/share/steel/cogs/term/` so edits apply without reinstalling.

## Troubleshooting

### Plugin fails to load / update doesn't take effect

The native library must be built against the same Steel revision as Helix and installed into the `STEEL_HOME` used by Helix.

## Acknowledgments

- [steel-pty](https://github.com/mattwparas/steel-pty) original rust dylib
- [steel-pty fork](https://github.com/bojohnson5/steel-pty.git) fork kebab/term's dylib was based on
- [@mattwparas](https://github.com/mattwparas/) for steel-pty, the Steel plugin system, and the wezterm-term fork
- The [wezterm](https://github.com/wez/wezterm) project for the VT emulator
- [portable-pty](https://github.com/wez/portable-pty) for cross-platform PTY handling

## Contributing

Contributors are always welcome. Feel free to grab an [issue](https://github.com/gravures/kebab.hx/issues) to work on or make a suggested improvement. If you wish to contribute, please read the [Contribution Guide](CONTRIBUTING.md) and [Code of Conduct](CODE_OF_CONDUCT.md). <!-- rumdl-disable-line MD013 -->


## License

Use of this repository is authorized under the [GPL-3.0](https://github.com/gravures/kebab.hx/LICENSE).
