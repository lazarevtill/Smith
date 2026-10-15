"""Independent Python stdlib checks of the synthetic generated PPTX package.

This checks ZIP, XML, references and visible text, not Office rendering.
Run from repository root: python3 tests/media_pptx_structure.py
"""
import json
import pathlib
import posixpath
import xml.etree.ElementTree as ET
import zipfile

fixture = pathlib.Path(__file__).resolve().parents[1] / "crates/smith-media/tests/fixtures/unicode.pptx"
ns = {
    "r": "http://schemas.openxmlformats.org/package/2006/relationships",
    "p": "http://schemas.openxmlformats.org/presentationml/2006/main",
    "a": "http://schemas.openxmlformats.org/drawingml/2006/main",
}
with zipfile.ZipFile(fixture) as package:
    assert package.testzip() is None
    names = package.namelist()
    assert len(names) == len(set(names))
    roots = {name: ET.fromstring(package.read(name)) for name in names}
    for name, root in roots.items():
        if not name.endswith(".rels"):
            continue
        directory = "" if name == "_rels/.rels" else name.split("/_rels/")[0]
        ids = set()
        for relationship in root.findall("r:Relationship", ns):
            assert relationship.attrib["Id"] not in ids
            ids.add(relationship.attrib["Id"])
            assert relationship.attrib.get("TargetMode", "Internal") == "Internal"
            target = posixpath.normpath(posixpath.join(directory, relationship.attrib["Target"]))
            assert target in roots, (name, target)
    presentation = roots["ppt/presentation.xml"]
    assert len(presentation.findall("p:sldIdLst/p:sldId", ns)) == 2
    size = presentation.find("p:sldSz", ns)
    assert int(size.attrib["cx"]) * 9 == int(size.attrib["cy"]) * 16
    visible = []
    for number in (1, 2):
        visible.extend(n.text or "" for n in roots[f"ppt/slides/slide{number}.xml"].findall(".//a:t", ns))
    assert "Привет, мир! Ёж" in visible
    assert "00123 & <safe> — café" in visible
    assert "9007199254740993.00" in visible
    source = json.loads(roots["customXml/item1.xml"].text)
    assert source["headers"] == ["Title", "Detail"]
    assert source["rows"][1][1] == {"type": "number", "value": "9007199254740993.00"}
print("PPTX structure PASS: all internal relationships resolve; two 16:9 slides preserve Unicode and exact numeric text")
