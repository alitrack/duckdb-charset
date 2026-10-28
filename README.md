# duckdb-charset

A native DuckDB loadable extension for **character-encoding detection, transcoding, and mojibake repair in SQL**: guess the encoding of a `BLOB` of raw bytes, decode it to UTF-8, encode a string back to a target codec, check UTF-8 validity, and repair the classic "UTF-8 misread as Windows-1252" garble.

Built on two battle-tested pure-Rust libraries that ship with Firefox:

- **`chardetng`** — the heuristic encoding *detector* (a Rust port of the Mozilla chardet-style guesser).
- **`encoding_rs`** — the WHATWG Encoding Standard *codec* library (decodes/encodes every web-platform label, with lossy U+FFFD / HTML-ref handling for unmappable input).

The engine is ported verbatim from the MIT-licensed [Query-farm `vgi-charset`](https://github.com/Query-farm/vgi-charset) VGI worker; only the Arrow IPC transport layer was replaced with direct DuckDB C-API vector I/O. No VGI, no Arrow IPC, no JSON shims.

## Functions

| Function | Signature | Returns | Notes |
| --- | --- | --- | --- |
| `detect_encoding` | `(bytes BLOB)` | `VARCHAR` | BOM first, then chardetng. Empty/NULL → NULL |
| `detect_confidence` | `(bytes BLOB)` | `DOUBLE` | `[0,1]` decode-cleanliness proxy. Empty → 0.0 |
| `is_valid_utf8` | `(bytes BLOB)` | `BOOLEAN` | strict `from_utf8` check |
| `to_utf8` | `(bytes BLOB)` | `VARCHAR` | decode with *detected* encoding. Undecodable → U+FFFD |
| `to_utf8_from` | `(bytes BLOB, label VARCHAR)` | `VARCHAR` | decode with *explicit* label. Unknown label → SQL error |
| `transcode` | `(text VARCHAR, to_encoding VARCHAR)` | `BLOB` | encode a string to bytes. Unknown label → SQL error |
| `fix_mojibake` | `(text VARCHAR)` | `VARCHAR` | repair UTF-8→1252 double-encoding; no-op if unsure |
| `supported_encodings` | `()` | `table(encoding)` | the 40 WHATWG/`encoding_rs` canonical labels |

### NULL / error policy
- `to_utf8` / `to_utf8_from` never error on *dirty* bytes — undecodable bytes become `U+FFFD` (clean decode of bad data). Only an **unknown encoding label** raises a SQL error.
- `detect_encoding` / `to_utf8` return NULL for empty input (nothing to detect); `detect_confidence` returns `0.0`.
- `fix_mojibake` is conservative: it repairs only when the re-decode provably reduces mojibake markers, otherwise it returns the input unchanged.

## Examples

```sql
-- Guess the encoding of a blob of windows-1252 "café" bytes and read it back
SELECT detect_encoding(x'636166e9')   AS detected;  -- windows-1252
SELECT to_utf8(x'636166e9')           AS text;      -- café

-- Explicit-label decode (e.g. a Shift_JIS byte stream)
SELECT to_utf8_from(x'93fa967b8cea', 'shift_jis')   AS text;  -- 日本語

-- Encode for export (café -> windows-1252 bytes, round-trips losslessly)
SELECT transcode('café', 'windows-1252')::VARCHAR   AS bytes; -- caf\xe9
SELECT to_utf8_from(transcode('café','windows-1252'), 'windows-1252') AS back; -- café

-- Repair garble: "CafÃ©" is what you get when UTF-8 "Café" is misread as 1252
SELECT fix_mojibake('CafÃ©')   AS fixed;  -- Café
SELECT fix_mojibake('Café')    AS noop;   -- Café (unchanged)

-- Validate and enumerate
SELECT is_valid_utf8(x'636166e9');               -- false (0xE9 alone is not UTF-8)
SELECT count(*) FROM supported_encodings();      -- 40
```

## Build

```bash
make release            # builds libcharset.so (cdylib)
# then stamp the loadable-extension metadata footer:
cargo run --bin append_metadata -- \
  target/release/libcharset.so target/release/charset.duckdb_extension \
  --abi-type C_STRUCT --extension-version v0.1.0 --duckdb-version v1.2.0 \
  --platform linux_amd64
```

Load it (unsigned, for local builds):

```sql
LOAD 'target/release/charset.duckdb_extension';
```

## Tests

`cargo test --release` runs the SQL-level integration suite (10 tests) against a real in-memory DuckDB with the built extension loaded.

## License

MIT — see [LICENSE](LICENSE). The core engine derives from
[Query-farm/vgi-charset](https://github.com/Query-farm/vgi-charset) (MIT).
