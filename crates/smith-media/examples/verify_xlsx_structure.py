"""Independent stdlib ZIP/XML checks, not a spreadsheet application/render test."""
from pathlib import Path
from zipfile import ZipFile
import posixpath
import xml.etree.ElementTree as ET

FIXTURES = Path(__file__).resolve().parents[1] / "tests" / "fixtures"
MAIN = "{http://schemas.openxmlformats.org/spreadsheetml/2006/main}"
REL = "{http://schemas.openxmlformats.org/package/2006/relationships}"

for name in ("cells.xlsx", "shared-strings.xlsx"):
    with ZipFile(FIXTURES / name) as archive:
        assert archive.testzip() is None
        names = set(archive.namelist())
        assert len(names) == len(archive.infolist())
        parts = {part: ET.fromstring(archive.read(part)) for part in names}
        for path, root in parts.items():
            assert root.find(f".//{MAIN}f") is None
            if path.endswith(".rels"):
                owner_dir = "" if path == "_rels/.rels" else path.split("/_rels/")[0]
                for relationship in root.findall(f"{REL}Relationship"):
                    assert relationship.get("TargetMode", "Internal") == "Internal"
                    target = relationship.attrib["Target"]
                    assert ":" not in target and "\\" not in target
                    resolved = posixpath.normpath(posixpath.join(owner_dir, target)).lstrip("/")
                    assert resolved in names
        workbook = parts["xl/workbook.xml"]
        cells = {c.attrib["r"]: c for c in parts["xl/worksheets/sheet1.xml"].iter(f"{MAIN}c")}
        if name == "cells.xlsx":
            assert cells["A2"].attrib["t"] == "inlineStr"
            assert cells["A2"].find(f"{MAIN}is/{MAIN}t").text == "00123"
            assert cells["A3"].attrib["t"] == "inlineStr"
            assert cells["A3"].find(f"{MAIN}is/{MAIN}t").text == "=SUM(A1:A2)"
            assert cells["B2"].attrib["t"] == "n"
            assert cells["B2"].find(f"{MAIN}v").text == "123.50"
            assert cells["B3"].attrib["t"] == "b"
            assert cells["B3"].find(f"{MAIN}v").text == "1"
            assert cells["B6"].attrib["t"] == "e"
            assert cells["B6"].find(f"{MAIN}v").text == "#N/A"
            assert "_x005F_x0041_" in cells["A4"].find(f"{MAIN}is/{MAIN}t").text
            assert workbook.findall(f"{MAIN}sheets/{MAIN}sheet")[1].attrib["state"] == "hidden"
        else:
            assert workbook.find(f"{MAIN}workbookPr").attrib["date1904"] == "1"
            assert cells["C4"].find(f"{MAIN}v").text == "9007199254740993"
            assert len(list(parts["xl/sharedStrings.xml"].iter(f"{MAIN}si"))) == 3
        print(f"{name}: ZIP CRC, XML, internal relationships, typed-cell structure PASS")

print("Structural validation only; Excel/LibreOffice/openpyxl not exercised.")
