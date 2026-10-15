# Bundled PDF font

Noto Sans Regular is embedded in generated PDF files. It is a third-party font asset under the adjacent SIL Open Font License 1.1, not an application source-code license.

- Official repository: https://github.com/notofonts/noto-fonts (formerly googlefonts/noto-fonts)
- Pinned commit: `ffebf8c1ee449e544955a7e813c54f9b73848eac`
- Font source: https://raw.githubusercontent.com/notofonts/noto-fonts/ffebf8c1ee449e544955a7e813c54f9b73848eac/hinted/ttf/NotoSans/NotoSans-Regular.ttf
- License source: https://raw.githubusercontent.com/notofonts/noto-fonts/ffebf8c1ee449e544955a7e813c54f9b73848eac/LICENSE
- Font SHA-256: `b85c38ecea8a7cfb39c24e395a4007474fa5a4fc864f6ee33309eb4948d232d5`
- License SHA-256: `0dab92d0544f7b233403f14b84a663bdbfa746982eda629e7f4f9ffe1b036feb`
- Fetched with explicit owner authorization on 2026-10-15. No runtime font fetch exists.
- A test checks every Russian alphabet glyph, including Ё/ё. Synthetic PDF text was independently extracted by Apple PDFKit with Russian, leading zeros and long numeric text intact. Complex shaping scripts, combining marks and missing glyphs produce typed unsupported errors.
