// DuckDB loadable extension: character-encoding detection, transcoding, and
// mojibake repair, written in SQL.
//
// This file is a thin quack-rs binding layer over `charset.rs` (a pure-Rust
// engine built on `encoding_rs` — the WHATWG codec library — and `chardetng` —
// Firefox's heuristic detector). The engine was ported verbatim from
// Query-farm's `vgi-charset` VGI worker (MIT); only the Arrow IPC transport
// layer was removed and replaced with direct C-API vector I/O.
//
// Function surface:
//
//   detect_encoding(bytes BLOB)   -> VARCHAR   (NULL/empty -> NULL)
//   detect_confidence(bytes BLOB) -> DOUBLE    (empty -> 0.0)
//   is_valid_utf8(bytes BLOB)     -> BOOLEAN
//   to_utf8(bytes BLOB)           -> VARCHAR   (NULL/empty -> NULL; decodes with
//                                                      detected encoding)
//   to_utf8_from(bytes, label)    -> VARCHAR   (unknown label -> SQL error)
//   transcode(text, to_encoding)  -> BLOB      (unknown label -> SQL error)
//   fix_mojibake(text)            -> VARCHAR   (no-op if not 1252-mojibake)
//   supported_encodings()         -> table(encoding)
//
// Semantics (inherited from vgi-charset):
//   * Detection checks a Unicode BOM first (UTF-8/16LE/16BE are explicit and
//     trusted), then falls back to the chardetng heuristic. Empty input has no
//     encoding to detect -> NULL (detect) / 0.0 (confidence) / NULL (to_utf8).
//   * `to_utf8` / `to_utf8_from` decode with encoding_rs; bytes that cannot be
//     represented in the target encoding become U+FFFD (a clean decode of dirty
//     data, never an error). A *recognized* label that contains undecodable
//     bytes is NOT an error — only an **unknown label** raises a SQL error.
//   * `transcode` (the inverse: encode a UTF-8 string into bytes) likewise
//     errors only on an unknown label.
//   * `fix_mojibake` is conservative: it repairs the classic "UTF-8 misread as
//     Windows-1252" double-encoding only when it provably improves the text,
//     and returns the input verbatim otherwise.

use libduckdb_sys::duckdb_vector_size;
use quack_rs::connection::{Connection, Registrar};
use quack_rs::error::ExtensionError;
use quack_rs::scalar::{ScalarFunctionBuilder, ScalarFunctionInfo};
use quack_rs::table::{BindInfo, FfiInitData, InitInfo, TableFunctionBuilder};
use quack_rs::types::TypeId;
use quack_rs::vector::{VectorReader, VectorWriter};

mod charset;

use charset::{confidence, detect, fix_mojibake, is_valid_utf8, supported_encodings, to_utf8, to_utf8_from, transcode};

// ─────────────────────────────────────────────────────────────────────────────
// Scalars: detection (BLOB -> VARCHAR / DOUBLE / BOOLEAN)
// ─────────────────────────────────────────────────────────────────────────────

quack_rs::scalar_callback!(detect_encoding_fn, |_info, input, output| {
    unsafe {
        let bytes = VectorReader::new(input, 0);
        let row_count = bytes.row_count();
        let mut writer = VectorWriter::new(output);
        for row in 0..row_count {
            match bytes.is_valid(row) {
                true => match detect(bytes.read_blob(row)) {
                    Some(label) => writer.write_varchar(row, label),
                    None => writer.set_null(row), // empty input
                },
                false => writer.set_null(row),
            }
        }
    }
});

quack_rs::scalar_callback!(detect_confidence_fn, |_info, input, output| {
    unsafe {
        let bytes = VectorReader::new(input, 0);
        let row_count = bytes.row_count();
        let mut writer = VectorWriter::new(output);
        for row in 0..row_count {
            if !bytes.is_valid(row) {
                writer.set_null(row);
                continue;
            }
            writer.write_f64(row, confidence(bytes.read_blob(row)));
        }
    }
});

quack_rs::scalar_callback!(is_valid_utf8_fn, |_info, input, output| {
    unsafe {
        let bytes = VectorReader::new(input, 0);
        let row_count = bytes.row_count();
        let mut writer = VectorWriter::new(output);
        for row in 0..row_count {
            if !bytes.is_valid(row) {
                writer.set_null(row);
                continue;
            }
            writer.write_bool(row, is_valid_utf8(bytes.read_blob(row)));
        }
    }
});

// ─────────────────────────────────────────────────────────────────────────────
// Scalars: to_utf8 (BLOB -> VARCHAR), to_utf8_from (BLOB, VARCHAR -> VARCHAR)
// ─────────────────────────────────────────────────────────────────────────────

quack_rs::scalar_callback!(to_utf8_fn, |_info, input, output| {
    unsafe {
        let bytes = VectorReader::new(input, 0);
        let row_count = bytes.row_count();
        let mut writer = VectorWriter::new(output);
        for row in 0..row_count {
            match bytes.is_valid(row) {
                true => match to_utf8(bytes.read_blob(row)) {
                    Some(s) => writer.write_varchar(row, &s),
                    None => writer.set_null(row), // empty input
                },
                false => writer.set_null(row),
            }
        }
    }
});

quack_rs::scalar_callback!(to_utf8_from_fn, |info, input, output| {
    unsafe {
        let bytes = VectorReader::new(input, 0);
        let label = VectorReader::new(input, 1);
        let row_count = bytes.row_count();
        let mut writer = VectorWriter::new(output);
        let fn_info = ScalarFunctionInfo::new(info);
        for row in 0..row_count {
            if !bytes.is_valid(row) || !label.is_valid(row) {
                writer.set_null(row);
                continue;
            }
            match to_utf8_from(bytes.read_blob(row), label.read_str(row)) {
                Ok(s) => writer.write_varchar(row, &s),
                // Unknown encoding label -> loud, recoverable SQL error.
                Err(e) => {
                    fn_info.set_error(&format!("to_utf8_from: {e}"));
                    return;
                }
            }
        }
    }
});

// ─────────────────────────────────────────────────────────────────────────────
// Scalars: transcode (VARCHAR, VARCHAR -> BLOB) ; fix_mojibake (VARCHAR)
// ─────────────────────────────────────────────────────────────────────────────

quack_rs::scalar_callback!(transcode_fn, |info, input, output| {
    unsafe {
        let text = VectorReader::new(input, 0);
        let label = VectorReader::new(input, 1);
        let row_count = text.row_count();
        let mut writer = VectorWriter::new(output);
        let fn_info = ScalarFunctionInfo::new(info);
        for row in 0..row_count {
            if !text.is_valid(row) || !label.is_valid(row) {
                writer.set_null(row);
                continue;
            }
            match transcode(text.read_str(row), label.read_str(row)) {
                Ok(b) => writer.write_blob(row, &b),
                Err(e) => {
                    fn_info.set_error(&format!("transcode: {e}"));
                    return;
                }
            }
        }
    }
});

quack_rs::scalar_callback!(fix_mojibake_fn, |_info, input, output| {
    unsafe {
        let text = VectorReader::new(input, 0);
        let row_count = text.row_count();
        let mut writer = VectorWriter::new(output);
        for row in 0..row_count {
            match text.is_valid(row) {
                true => writer.write_varchar(row, &fix_mojibake(text.read_str(row))),
                false => writer.set_null(row),
            }
        }
    }
});

// ─────────────────────────────────────────────────────────────────────────────
// Table function: supported_encodings() -> (encoding)
// ─────────────────────────────────────────────────────────────────────────────

struct EncState {
    next: usize,
    total: usize,
}

quack_rs::table_bind_callback!(enc_bind, |info| {
    unsafe {
        BindInfo::new(info)
            .add_result_column("encoding", TypeId::Varchar)
            .set_cardinality(supported_encodings().len() as u64, true);
    }
});

quack_rs::table_init_callback!(enc_init, |info| {
    unsafe {
        let total = supported_encodings().len();
        InitInfo::new(info).set_max_threads(1);
        FfiInitData::<EncState>::set(info, EncState { next: 0, total });
    }
});

quack_rs::table_scan_callback!(enc_scan, |info, output| {
    unsafe {
        let chunk = quack_rs::data_chunk::DataChunk::from_raw(output);
        let Some(state) = FfiInitData::<EncState>::get_mut(info) else {
            chunk.set_size(0);
            return;
        };
        let all = supported_encodings();
        if state.next >= state.total {
            chunk.set_size(0);
            return;
        }
        let batch = (state.total - state.next).min(duckdb_vector_size() as usize);
        let mut w = chunk.writer(0);
        for i in 0..batch {
            w.write_varchar(i, all[state.next + i]);
        }
        state.next += batch;
        chunk.set_size(batch)
    }
});

// ─────────────────────────────────────────────────────────────────────────────
// Registration + entry point
// ─────────────────────────────────────────────────────────────────────────────

unsafe fn register_all(con: &Connection) -> Result<(), ExtensionError> {
    // detect_encoding(bytes) -> VARCHAR
    con.register_scalar(
        ScalarFunctionBuilder::new("detect_encoding")
            .param(TypeId::Blob)
            .returns(TypeId::Varchar)
            .function(detect_encoding_fn),
    )?;

    // detect_confidence(bytes) -> DOUBLE
    con.register_scalar(
        ScalarFunctionBuilder::new("detect_confidence")
            .param(TypeId::Blob)
            .returns(TypeId::Double)
            .function(detect_confidence_fn),
    )?;

    // is_valid_utf8(bytes) -> BOOLEAN
    con.register_scalar(
        ScalarFunctionBuilder::new("is_valid_utf8")
            .param(TypeId::Blob)
            .returns(TypeId::Boolean)
            .function(is_valid_utf8_fn),
    )?;

    // to_utf8(bytes) -> VARCHAR
    con.register_scalar(
        ScalarFunctionBuilder::new("to_utf8")
            .param(TypeId::Blob)
            .returns(TypeId::Varchar)
            .function(to_utf8_fn),
    )?;

    // to_utf8_from(bytes, label) -> VARCHAR
    con.register_scalar(
        ScalarFunctionBuilder::new("to_utf8_from")
            .param(TypeId::Blob)
            .param(TypeId::Varchar)
            .returns(TypeId::Varchar)
            .function(to_utf8_from_fn),
    )?;

    // transcode(text, to_encoding) -> BLOB
    con.register_scalar(
        ScalarFunctionBuilder::new("transcode")
            .param(TypeId::Varchar)
            .param(TypeId::Varchar)
            .returns(TypeId::Blob)
            .function(transcode_fn),
    )?;

    // fix_mojibake(text) -> VARCHAR
    con.register_scalar(
        ScalarFunctionBuilder::new("fix_mojibake")
            .param(TypeId::Varchar)
            .returns(TypeId::Varchar)
            .function(fix_mojibake_fn),
    )?;

    // supported_encodings() -> table(encoding)
    con.register_table(
        TableFunctionBuilder::new("supported_encodings")
            .bind(enc_bind)
            .init(enc_init)
            .scan(enc_scan),
    )?;

    Ok(())
}

quack_rs::entry_point_v2!(charset_init_c_api, |con| register_all(con));
