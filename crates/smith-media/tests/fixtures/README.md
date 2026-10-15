# Synthetic attachment fixtures

All fixture contents are newly authored for this crate. They contain no private/user material and were not copied from the original Smith application.

- `unicode.txt`: multilingual text, decomposed/composed accents, emoji, LF newlines.
- `quoted.csv`: three-column records with a quoted comma, doubled quotes, empty field, multiline field and inert formula-looking string.
- `precise.json`: large integer, exact decimal fraction, Unicode, booleans/null and a literal reserved-looking object key.
- `pixels.png`, `pixels.jpg`, `pixels.bmp`, `pixels.webp`: deterministic 3 × 2 pixel rasters, encoded by public `image` codecs.
- `two-frames.gif`: two 3 × 2 solid-color frames; the loader must validate both.
- `animated.png`: valid 1 × 1 two-frame APNG, explicitly rejected as unsupported.
- `cells.xlsx`, `cells.expected.json`: generated typed workbook with leading zeros, formula-looking literal text, precision, Unicode/CRLF/XML escapes, empty/boolean/error cells, date number format and a hidden sheet.
- `shared-strings.xlsx`: independently authored Python ZIP/XML workbook with sparse rows, rich/shared strings, Excel escapes, 1904 date system and inherited number formats.
- `animated.webp`: two-frame WebP using the synthetic lossless raster, explicitly rejected as unsupported.

From the repository root, regenerate only these public test fixtures:

```sh
cargo run --manifest-path crates/smith-media/Cargo.toml --offline --example generate_fixtures
python3 crates/smith-media/examples/generate_rejection_fixtures.py
cargo run --manifest-path crates/smith-media/Cargo.toml --locked --offline --example generate_workbook_fixtures
python3 crates/smith-media/examples/generate_shared_string_fixture.py
python3 crates/smith-media/examples/verify_xlsx_structure.py
```

No model, network call, source screenshot, production database or original code asset is involved.
