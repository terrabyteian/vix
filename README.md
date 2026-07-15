# vix — slim vim-motion editor

A single-binary, no-config, opinionated modal editor. Vim grammar (operator + motion, text objects, counts, `f/t`, `/n/N`, `.` repeat) with modern batteries: tree-sitter, LSP, fuzzy pickers, live regex search — all baked in.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/terrabyteian/vix/master/install.sh | sh
```

Pin a specific version:

```sh
curl -fsSL https://raw.githubusercontent.com/terrabyteian/vix/master/install.sh | VIX_VERSION=v0.4.0 sh
```

**Build from source** (requires Rust):

```sh
cargo install --path crates/app
```

## What it does

`vix` opens files, navigates, and edits them with traditional Vim motions. It ships with tree-sitter highlighting for 13 languages, an LSP client for 4 of those, and a unified fuzzy file/grep picker — none of which require a config file or a plugin manager. There is no Vimscript, no Lua, no plugin system. All behaviour is baked into the binary.

## Quickstart

```sh
vix                  # open the file picker in the current directory
vix path/to/dir/     # chdir into the directory and open the picker
vix path/to/file     # open a single file (creates an empty buffer if missing)
vix --help           # one-screen help
vix --version
```

Inside the editor, `:help` opens the in-binary help index; `:help <topic>` jumps to a topic.

## Modes

| Mode | Enter from Normal | Notes |
|---|---|---|
| Normal | `<Esc>` from any mode | The default. Motions, operators, text objects. |
| Insert | `i` `a` `I` `A` `o` `O` `s` `S` `c<motion>` | Plain typing. One Insert session = one undo. |
| Visual | `v` | Charwise selection. |
| Visual-line | `V` | Linewise selection. |
| Command | `:` | Ex command line at the bottom. |

## Motions, operators, text objects

| Category | Bindings |
|---|---|
| Cursor | `h j k l` `0 ^ $` `gg G` `w W b B e E` `{ }` `( )` `f{c} F{c} t{c} T{c} ; ,` `% ` |
| Search | `/pat` `?pat` `n N` `* #` |
| Operators | `d c y p P < > = ~` `gu gU` |
| Text objects | `iw aw` `i" a"` `i' a'` `` i` a` `` `i( a(` `i[ a[` `i{ a{` `i< a<` `it at` |
| Counts | Prefix anything: `3dw`, `5j`, `c2w` |
| Repeat | `.` repeats the last change with intent (not raw keys) |
| Registers | `"` (unnamed), `"+` (system clipboard via OSC 52) |
| Undo / redo | `u` / `<C-r>` |
| Jump list | `<C-o>` / `<C-i>` (alias `go` / `gi`); `:jumps` opens the picker |

## Pickers

`<Space>` is the leader. Held picker query is preserved when you `<Tab>` between Files and Grep.

| Trigger | Picker |
|---|---|
| `<Space>f` or `:Files` | Fuzzy file finder (`ignore`-walked; honours `.gitignore`) |
| `<Space>g` or `:Grep [pat]` | Live regex grep across the project (ripgrep guts) |
| `<Tab>` (in picker) | Toggle Files ⇆ Grep, preserving the query |
| `:Buffers` / `:ls` | Open buffers |
| `:Symbols` | Tree-sitter outline of the current buffer |
| `:jumps` | Jump list |

Inside a picker — it's single-mode, fzf-style: you're always typing, and
everything else lives on a non-printable key or a Ctrl/Alt chord so it never
collides with a query character.

| Key | Action |
|---|---|
| Type | Filter the query (printable keys, no Ctrl/Alt) |
| `<Backspace>` | Delete the last query character |
| `<C-u>` | Clear the query |
| `<C-w>` | Delete the trailing word from the query |
| `↑` / `↓` / `<C-j>` / `<C-k>` / mouse scroll | Move selection |
| `<PageUp>` / `<PageDown>` | Move selection by a page |
| `<Home>` / `<End>` | Jump to the first / last match |
| `<Enter>` | Open the selected entry (or every marked entry, if any) |
| `<Tab>` | Toggle Files ⇆ Grep, preserving the query |
| `<C-Space>` | Mark the current row for batch-open, then advance |
| `<A-c>` | Clear all marks |
| `<C-s>` / `<C-q>` / `<A-q>` / `<C-r>` / `<A-r>` | Buffers only: save / close / force-close / reload / force-reload |
| First click on a row | Focus the row |
| Second click on the focused row | Open it |
| `<Esc>` / `<C-c>` | Close the picker immediately |

## Buffers

vix uses Vim's `hidden` semantics — parked buffers can be dirty.

| Key | Action |
|---|---|
| `<Tab>` / `<S-Tab>` | Cycle to next / previous buffer (alt-tab style) |
| `:e <path>` | Open a file (parks the active buffer if any) |
| `:bn` / `:bp` | Next / previous buffer |
| `:b <n\|name>` | Switch by index or path substring |
| `:bd` / `:bd!` | Close the active buffer (`!` discards unsaved changes) |
| `:ls` | Open the buffer picker |

## Ex commands

| Command | Action |
|---|---|
| `:w` | Format-on-save (if LSP) and write |
| `:wq` / `:x` | Write and close |
| `:q` / `:q!` | Close buffer (force-discard with `!`) |
| `:qa` / `:qa!` | Quit vix (force with `!`) — aliases `:qall` / `:qall!` |
| `:e <path>` | Open a file |
| `:b <n\|name>` | Switch buffer |
| `:bn` / `:bp` / `:bd[!]` | Next / prev / delete buffer |
| `:ls` / `:Buffers` | Buffer picker |
| `:Files` | File picker |
| `:Grep [pat]` | Grep picker, optionally pre-filled |
| `:Symbols` | Symbol picker for the current buffer |
| `:jumps` | Jump list picker |
| `:%s/pat/rep/[g][i]` | Substitute over the whole file |
| `:s/pat/rep/[g][i]` | Substitute on the current line |
| `:noh` / `:nohlsearch` | Clear search highlight |
| `:fmt` / `:format` | LSP format the buffer |
| `:action` / `:ca` | LSP code action |
| `:rename <new-name>` | LSP rename at cursor |
| `:help [topic]` | Open the in-binary help |

## Languages

**Tree-sitter highlighting** (13): Rust, Python, JavaScript (incl. JSX/MJS/CJS), TypeScript, TSX, Go, Markdown, JSON, TOML, HTML, CSS, Bash, HCL family (Terraform `.tf` / `.tfvars`, OpenTofu `.tofu` / `.tofuvars`, Packer `.pkr.hcl`, Nomad `.nomad`, bare `.hcl`).

**Symbol picker** (5 of those): Rust, Python, JavaScript, TypeScript/TSX, Go.

**LSP** (4 servers — must already be on `$PATH`):

| Server | Languages |
|---|---|
| `rust-analyzer` | `.rs` |
| `pyright-langserver` | `.py` |
| `typescript-language-server` | `.ts` `.tsx` `.js` `.jsx` |
| `gopls` | `.go` |

LSP features: diagnostics in the gutter + statusline, hover (`K`), goto-definition (`gd`), completion popup, format-on-save, rename, code actions.

vix does **not** bundle or auto-install language servers. Install them yourself:

```sh
brew install rust-analyzer
brew install gopls
npm install -g pyright typescript typescript-language-server
```

If a server is missing, vix runs without LSP for that buffer — no popup, just no features.

## Configuration

There is none, by design. No `~/.vixrc`, no plugin system, no Vimscript, no Lua. Behaviour is baked into the binary; upgrades come through new releases.

Deliberate non-goals: multi-cursor, Windows support, a config file, a plugin system, bundled LSP binaries.

## Supported platforms

| OS | Architecture |
|---|---|
| macOS | arm64 |
| Linux | x86_64 |
| Linux | arm64 |

Intel Macs can run the arm64 binary via Rosetta 2.

## License

MIT.
