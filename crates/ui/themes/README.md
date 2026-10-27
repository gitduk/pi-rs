Catppuccin Mocha and Latte from [catppuccin/bat](https://github.com/catppuccin/bat/tree/main/themes),
under the accompanying MIT [LICENSE](LICENSE). These are the default dark and
light palettes used by [Codex](https://github.com/openai/codex/blob/main/codex-rs/tui/src/render/highlight.rs).

The themes are embedded in the binary. Rendering never reads theme files from
disk. The palettes colour code blocks; prose keeps the terminal foreground,
headings use the palette's six level colours without `#` markers, bold text is
red, and italic keeps its foreground. Explicit pi styles override the
corresponding defaults.
