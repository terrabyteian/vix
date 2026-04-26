# Fixture: Markdown

This file exercises **markdown** highlighting in *vix*.

## Inline elements

Plain text with `inline code`, **bold**, *italic*, and ~~strikethrough~~.
A [link to anthropic](https://anthropic.com) and an autolink: <https://example.com>.

## Lists

- bullet one
- bullet two
  - nested
  - nested again
- bullet three

1. ordered one
2. ordered two
3. ordered three

## Code block

```rust
fn greet(name: &str) -> String {
    format!("hi, {name}!")
}
```

```python
def double(x: int) -> int:
    return x * 2
```

## Blockquote

> "The best way to predict the future is to invent it."
> — Alan Kay

## Table

| Lang   | Highlight | LSP |
|--------|-----------|-----|
| Rust   | yes       | yes |
| Python | yes       | yes |
| Bash   | yes       | no  |

## Horizontal rule

---

Trailing paragraph with a footnote-ish marker[^1].

[^1]: Not all markdown grammars handle footnotes.
