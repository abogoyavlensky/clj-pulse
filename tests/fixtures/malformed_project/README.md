# malformed_project

A fixture for the malformed-input e2e tests.

`src/bad_bytes.clj` is **not valid UTF-8** on purpose — it holds a lone `0xFF`
byte and a latin-1 `é` — so do not open it in an editor that will "fix" the
encoding on save, and do not let a formatter rewrite it. The scanner must skip
an unreadable file and index the rest of the project (`src/ok.clj`) anyway.

Regenerate the bytes with:

```
python3 -c 'open("src/bad_bytes.clj","wb").write(b"(ns malformed.bad-bytes)\n\n(def latin-1 \"caf\xe9\")\n\n(def lone-ff \"\xff\")\n")'
```
