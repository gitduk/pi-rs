Catppuccin Mocha and Latte from [catppuccin/bat](https://github.com/catppuccin/bat/tree/main/themes),
under the accompanying MIT [LICENSE](LICENSE). These are the default dark and
light palettes used by [Codex](https://github.com/openai/codex/blob/main/codex-rs/tui/src/render/highlight.rs).

The themes are embedded in the binary. Rendering never reads theme files from
disk. The palettes colour code blocks. The surrounding text uses their cool
colours: blue inline code, and teal italic text and quotes.
Bold text uses the theme green. Wide tables shown as source alternate column
text between blue and green to keep neighbouring columns distinct.
Prose keeps the terminal foreground. Headings omit `#` markers and follow
Codex's styles: red bold underlined H1, peach bold H2, yellow bold italic H3,
and yellow italic H4–H6. Their colours come from the theme's `markup.heading`
scopes, with the corresponding dark or light palette.
Explicit pi styles override the corresponding defaults.

The surrounding controls follow the semantic styles in
[Codex 0.165.0](https://github.com/openai/codex/blob/rust-v0.165.0/codex-rs/tui/src/style.rs):
ChatGPT blue on dark backgrounds and a darker blue on light ones.
Input and submitted text retain their original background shading.
Selected rows share the input background with bold theme-blue text;
success and failure keep the terminal's green and red.
The terminal keeps its own foreground and background for prose.
