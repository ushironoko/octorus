# tree-sitter-moonbit

MoonBit grammar for tree-sitter.

This crate provides Rust bindings for the [MoonBit](https://www.moonbitlang.com/) tree-sitter grammar.

## Usage

```rust
let code = r#"
fn main {
    println("Hello, MoonBit!")
}
"#;
let mut parser = tree_sitter::Parser::new();
let language = tree_sitter_moonbit::LANGUAGE;
parser
    .set_language(&language.into())
    .expect("Error loading MoonBit parser");
let tree = parser.parse(code, None).unwrap();
```

## Attribution

The grammar files (`src/parser.c`, `src/scanner.c`) and highlight queries (`queries/highlights.scm`) are from the [moonbitlang/tree-sitter-moonbit](https://github.com/moonbitlang/tree-sitter-moonbit) repository, licensed under Apache-2.0.

## License

Apache-2.0
