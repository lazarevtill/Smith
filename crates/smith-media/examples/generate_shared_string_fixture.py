"""Independent OOXML fixture producer using Python's ZIP/XML standard library."""
from pathlib import Path
from zipfile import ZipFile, ZIP_DEFLATED, ZipInfo

out = Path(__file__).resolve().parents[1] / "tests" / "fixtures" / "shared-strings.xlsx"
main = "http://schemas.openxmlformats.org/spreadsheetml/2006/main"
rel = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"
pkg = "http://schemas.openxmlformats.org/package/2006/relationships"
types = "http://schemas.openxmlformats.org/package/2006/content-types"
parts = {
    "[Content_Types].xml": f'''<Types xmlns="{types}"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/sharedStrings.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml"/><Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/></Types>''',
    "_rels/.rels": f'''<Relationships xmlns="{pkg}"><Relationship Id="workbook" Type="{rel}/officeDocument" Target="xl/workbook.xml"/></Relationships>''',
    "xl/_rels/workbook.xml.rels": f'''<Relationships xmlns="{pkg}"><Relationship Id="sheet" Type="{rel}/worksheet" Target="/xl/worksheets/sheet1.xml"/><Relationship Id="strings" Type="{rel}/sharedStrings" Target="sharedStrings.xml"/><Relationship Id="styles" Type="{rel}/styles" Target="styles.xml"/></Relationships>''',
    "xl/workbook.xml": f'''<workbook xmlns="{main}" xmlns:r="{rel}"><workbookPr date1904="1"/><sheets><sheet name="Sparse" sheetId="1" r:id="sheet"/></sheets></workbook>''',
    "xl/sharedStrings.xml": f'''<sst xmlns="{main}" count="3" uniqueCount="3"><si><t>00123</t></si><si><r><t>rich </t></r><r><rPr><b/></rPr><t>世界</t></r></si><si><t>_xD83D__xDC4B_ literal _x005F_x0041_</t></si></sst>''',
    "xl/styles.xml": f'''<styleSheet xmlns="{main}"><numFmts count="1"><numFmt numFmtId="170" formatCode="yyyy-mm-dd"/></numFmts><cellXfs count="3"><xf numFmtId="0"/><xf numFmtId="170"/><xf numFmtId="14"/></cellXfs></styleSheet>''',
    "xl/worksheets/sheet1.xml": f'''<worksheet xmlns="{main}"><dimension ref="A1:E8"/><cols><col min="5" max="5" style="2"/></cols><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c></row><row r="4" s="1" customFormat="1"><c r="B4" t="n"><v>100.25</v></c><c r="C4" t="n" s="0"><v>9007199254740993</v></c></row><row r="8"><c r="A8" t="s"><v>2</v></c><c r="E8" t="n"><v>200.5</v></c></row></sheetData></worksheet>''',
}
with ZipFile(out, "w") as archive:
    for name, body in sorted(parts.items()):
        info = ZipInfo(name, (1980, 1, 1, 0, 0, 0))
        info.compress_type = ZIP_DEFLATED
        info.external_attr = 0o100600 << 16
        archive.writestr(info, '<?xml version="1.0" encoding="UTF-8"?>' + body)
print(out.name)
