# Plan: Go-to-Definition for Library Symbols (V2)

## Context

The LSP indexes JAR symbols (V1, plan 001) but `goto_definition` returns `None` for them. V2 serves actual source from JARs using LSP 3.18 `workspace/textDocumentContent`, enabling jump-to and peek-definition for library code.

## Approach

Use the standard `workspace/textDocumentContent` provider with a `jar` URI scheme. When the editor requests a `jar:` URI, extract the source file from the JAR on-demand and return it as plain text. Tower-lsp 0.20's `LspService::build().custom_method()` handles the raw JSON-RPC method.

**URI format:** `jar:file:///path/to/lib.jar!/clojure/string.clj`

---

## Part 1: Custom Method Registration

Switch `src/main.rs` from `LspService::new()` to `LspService::build()`:

```rust
let (service, socket) = LspService::build(Backend::new)
    .custom_method("workspace/textDocumentContent", Backend::text_document_content)
    .finish();
```

Advertise the capability in `src/server.rs` `initialize` response via `experimental`:

```rust
experimental: Some(serde_json::json!({
    "textDocumentContentProvider": { "schemes": ["jar"] }
}))
```

LSP 3.18's `textDocumentContentProvider` isn't in `lsp-types` yet, so `experimental` is the correct fallback.

---

## Part 2: JAR Content Extraction (`src/jar_content.rs`)

New module with two functions:

```rust
pub fn parse_jar_uri(uri: &str) -> Result<(PathBuf, String)>
// Parses "jar:file:///path/to.jar!/entry.clj" into (jar_path, entry_path)

pub fn extract_content(jar_path: &Path, entry_path: &str) -> Result<String>
// Opens the JAR as ZIP, reads the named entry, returns source text
```

On-demand extraction keeps memory low. JAR reads are ~1ms so no caching needed.

---

## Part 3: Content Provider Handler (`src/server.rs`)

Add method on `Backend`:

```rust
async fn text_document_content(&self, params: TextDocumentContentParams)
    -> jsonrpc::Result<TextDocumentContentResult>
```

Where `TextDocumentContentParams` and `TextDocumentContentResult` are local structs (not in `lsp-types`):

```rust
#[derive(Deserialize)]
struct TextDocumentContentParams {
    uri: String,
}

#[derive(Serialize)]
struct TextDocumentContentResult {
    text: String,
}
```

Parses the `jar:` URI, calls `jar_content::extract_content`, returns the source.

---

## Part 4: Return `jar:` URIs from Definition Handler

Change `src/handlers/definition.rs` line 39:

```rust
// Before:
SymbolSource::Jar(_) => Ok(None),

// After:
SymbolSource::Jar(_) => {
    let jar_uri = format!("jar:file://{}", sym.file.display());
    let uri = Url::parse(&jar_uri)
        .map_err(|_| anyhow::anyhow!("invalid jar URI: {}", jar_uri))?;
    Ok(Some(GotoDefinitionResponse::Scalar(Location {
        uri,
        range: sym.name_range,
    })))
}
```

The `sym.file` field already stores the virtual path in `jar_path!/entry` format (set by `jar.rs` indexing).

---

## Error Handling

All errors return JSON-RPC error codes, never crash:

- JAR not on disk → `ContentModified` error
- Entry not in JAR → `InvalidParams` error
- URI parse failure → `InvalidParams` error
- ZIP read error → `InternalError` (logged)

---

## Changes Summary

| File | Change |
|------|--------|
| `src/main.rs` | Switch to `LspService::build()` with `.custom_method()` |
| `src/server.rs` | Add `experimental` capability, add `text_document_content` handler |
| `src/handlers/definition.rs` | Return `jar:` URI instead of `None` for JAR symbols |
| `src/jar_content.rs` | **New:** URI parsing + on-demand JAR content extraction |

No new dependencies needed — `zip` and `serde_json` are already present.

---

## Tests

### `src/jar_content.rs` — unit tests
- Parse valid `jar:` URI into components
- Reject malformed URIs
- Extract content from a test JAR (reuse `make_jar` pattern from `jar.rs`)
- Error on missing entry

### `tests/` — integration test
- Index a test JAR, verify `goto_definition` returns a `jar:` URI with correct range
- Call content extraction, verify source text matches

### Manual verification
- Open a Clojure project with deps in VS Code
- Ctrl-click library symbol → navigates to JAR source
- Peek definition → inline preview shows source
