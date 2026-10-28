//! SQL-level integration tests: LOAD the built `.duckdb_extension` into a real
//! in-memory DuckDB and exercise the full charset surface — 7 scalars (BLOB
//! in/out, VARCHAR, DOUBLE, BOOLEAN) + the supported_encodings() table.

use quack_rs::testing::InMemoryDb;

fn db() -> InMemoryDb {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/release/charset.duckdb_extension");
    assert!(path.exists(), "build first: make release -> {path:?}");
    let d = InMemoryDb::open_unsigned().expect("open in-memory db");
    d.execute_batch(&format!("LOAD '{}'", path.display())).expect("load extension");
    d
}

// windows-1252 bytes for "café" (é = 0xE9 in 1252).
const CAFE_1252: &str = "x'636166e9'";

#[test]
fn detect_windows_1252_cafe() {
    let d = db();
    assert_eq!(
        d.query_one::<String>(&format!("SELECT detect_encoding({CAFE_1252})")).unwrap(),
        "windows-1252"
    );
}

#[test]
fn detect_utf8() {
    let d = db();
    assert_eq!(
        d.query_one::<String>("SELECT detect_encoding(x'48c3a96c6c6f')").unwrap(), // "Héllo"
        "UTF-8"
    );
}

#[test]
fn detect_empty_is_null() {
    let d = db();
    assert_eq!(d.query_one::<Option<String>>("SELECT detect_encoding(x'')").unwrap(), None);
    assert_eq!(d.query_one::<Option<String>>("SELECT detect_encoding(NULL)").unwrap(), None);
}

#[test]
fn to_utf8_decodes_cafe() {
    let d = db();
    assert_eq!(
        d.query_one::<String>(&format!("SELECT to_utf8({CAFE_1252})")).unwrap(),
        "café"
    );
}

#[test]
fn to_utf8_from_explicit_shift_jis() {
    let d = db();
    // "日本語" in Shift_JIS.
    assert_eq!(
        d.query_one::<String>("SELECT to_utf8_from(x'93fa967b8cea', 'shift_jis')").unwrap(),
        "日本語"
    );
}

#[test]
fn to_utf8_from_unknown_label_errors() {
    let d = db();
    let err = d
        .query_one::<String>("SELECT to_utf8_from(x'616263', 'not-a-real-encoding')")
        .unwrap_err();
    assert!(err.to_string().contains("not-a-real-encoding"), "{err}");
}

#[test]
fn transcode_round_trip() {
    let d = db();
    // café -> windows-1252 bytes -> back to café.
    let encoded: String = d
        .query_one("SELECT transcode('café', 'windows-1252')::VARCHAR")
        .unwrap();
    assert!(encoded.to_lowercase().contains("e9"), "expected 0xE9 for é, got {encoded}");
    assert_eq!(
        d.query_one::<String>(&format!("SELECT to_utf8_from(transcode('café', 'windows-1252'), 'windows-1252')")).unwrap(),
        "café"
    );
}

#[test]
fn is_valid_utf8_checks() {
    let d = db();
    assert!(d
        .query_one::<bool>("SELECT is_valid_utf8(x'636166c3a9')") // "café" UTF-8
        .unwrap());
    assert!(!d
        .query_one::<bool>(&format!("SELECT is_valid_utf8({CAFE_1252})"))
        .unwrap()); // 0xE9 alone is not valid UTF-8
}

#[test]
fn fix_mojibake_cafe() {
    let d = db();
    // "CafÃ©" is the mojibake of "Café".
    assert_eq!(
        d.query_one::<String>("SELECT fix_mojibake('CafÃ©')").unwrap(),
        "Café"
    );
    // Clean text is a no-op.
    assert_eq!(d.query_one::<String>("SELECT fix_mojibake('Café')").unwrap(), "Café");
    assert_eq!(
        d.query_one::<String>("SELECT fix_mojibake('plain ascii')").unwrap(),
        "plain ascii"
    );
}

#[test]
fn supported_encodings_table() {
    let d = db();
    let n: i64 = d.query_one("SELECT count(*) FROM supported_encodings()").unwrap();
    assert!(n >= 30, "expected many encodings, got {n}");
    assert!(d
        .query_one::<bool>("SELECT bool_or(encoding = 'UTF-8') FROM supported_encodings()")
        .unwrap());
    assert!(d
        .query_one::<bool>("SELECT bool_or(encoding = 'Shift_JIS') FROM supported_encodings()")
        .unwrap());
}
