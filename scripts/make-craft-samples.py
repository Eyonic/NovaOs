#!/usr/bin/env python3
"""Write small sample documents for the Craft apps (scripts/build.sh puts them in the
image under /usr/share/nova/craft/samples/ when NOVA_CRAFT is set).

Minimal but valid files, made with the standard library only: a Word document (.docx),
an Excel workbook (.xlsx), a PowerPoint deck (.pptx), a PDF and a PNG image. All text is
written here; nothing is copied from elsewhere.

Usage: make-craft-samples.py <output directory>
"""
import os
import struct
import sys
import zipfile
import zlib

CT = "http://schemas.openxmlformats.org/package/2006/content-types"
REL = "http://schemas.openxmlformats.org/package/2006/relationships"
ODOC = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"
XML = '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'


def package(path, parts):
    """Write an OPC (OOXML) package: parts maps names to XML strings."""
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
        # [Content_Types].xml first, as Office writes it.
        z.writestr("[Content_Types].xml", parts.pop("[Content_Types].xml"))
        for name, data in parts.items():
            z.writestr(name, data)


def rels(*entries):
    body = "".join(f'<Relationship Id="{i}" Type="{t}" Target="{target}"/>' for i, t, target in entries)
    return f'{XML}<Relationships xmlns="{REL}">{body}</Relationships>'


def docx(path):
    w = "http://schemas.openxmlformats.org/wordprocessingml/2006/main"

    def para(text, style=None, bold=False):
        ppr = f'<w:pPr><w:pStyle w:val="{style}"/></w:pPr>' if style else ""
        rpr = "<w:rPr><w:b/></w:rPr>" if bold else ""
        return f'<w:p>{ppr}<w:r>{rpr}<w:t xml:space="preserve">{text}</w:t></w:r></w:p>'

    body = "".join([
        para("Nova Craft sample", "Title"),
        para("A Word document made for testing WordCraft on Nova.", "Heading1"),
        para("This paragraph is plain text. The next one is bold."),
        para("Bold text in a second paragraph.", bold=True),
        para("Type below this line and save with Ctrl+S."),
    ])
    styles = (f'{XML}<w:styles xmlns:w="{w}">'
              '<w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/>'
              '<w:rPr><w:sz w:val="24"/></w:rPr></w:style>'
              '<w:style w:type="paragraph" w:styleId="Title"><w:name w:val="Title"/><w:basedOn w:val="Normal"/>'
              '<w:rPr><w:b/><w:sz w:val="56"/></w:rPr></w:style>'
              '<w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/><w:basedOn w:val="Normal"/>'
              '<w:rPr><w:b/><w:color w:val="2F5597"/><w:sz w:val="32"/></w:rPr></w:style></w:styles>')
    package(path, {
        "[Content_Types].xml": f'{XML}<Types xmlns="{CT}">'
        '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
        '<Default Extension="xml" ContentType="application/xml"/>'
        '<Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>'
        '<Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>'
        "</Types>",
        "_rels/.rels": rels(("rId1", f"{ODOC}/officeDocument", "word/document.xml")),
        "word/_rels/document.xml.rels": rels(("rId1", f"{ODOC}/styles", "styles.xml")),
        "word/document.xml": f'{XML}<w:document xmlns:w="{w}"><w:body>{body}'
        '<w:sectPr><w:pgSz w:w="11906" w:h="16838"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440"/></w:sectPr>'
        "</w:body></w:document>",
        "word/styles.xml": styles,
    })


def xlsx(path):
    s = "http://schemas.openxmlformats.org/spreadsheetml/2006/main"
    rows = [("Item", "Quantity", "Price", "Total"), ("Apples", 4, 0.5, None), ("Bread", 1, 2.25, None),
            ("Coffee", 2, 6.0, None), ("Sum", None, None, "SUM")]
    xml_rows = []
    for r, row in enumerate(rows, 1):
        cells = []
        for c, v in enumerate(row):
            ref = "ABCD"[c] + str(r)
            if isinstance(v, str) and v != "SUM":
                cells.append(f'<c r="{ref}" t="inlineStr"><is><t>{v}</t></is></c>')
            elif isinstance(v, (int, float)):
                cells.append(f'<c r="{ref}"><v>{v}</v></c>')
            elif v == "SUM":
                cells.append(f'<c r="{ref}"><f>SUM(D2:D{r - 1})</f><v>{4 * 0.5 + 2.25 + 12.0}</v></c>')
            elif c == 3 and r > 1:
                cells.append(f'<c r="{ref}"><f>B{r}*C{r}</f><v>{row[1] * row[2]}</v></c>')
        xml_rows.append(f'<row r="{r}">{"".join(cells)}</row>')
    package(path, {
        "[Content_Types].xml": f'{XML}<Types xmlns="{CT}">'
        '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
        '<Default Extension="xml" ContentType="application/xml"/>'
        '<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>'
        '<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>'
        '<Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>'
        "</Types>",
        "_rels/.rels": rels(("rId1", f"{ODOC}/officeDocument", "xl/workbook.xml")),
        "xl/_rels/workbook.xml.rels": rels(("rId1", f"{ODOC}/worksheet", "worksheets/sheet1.xml"), ("rId2", f"{ODOC}/styles", "styles.xml")),
        "xl/styles.xml": f'{XML}<styleSheet xmlns="{s}"><fonts count="1"><font><sz val="11"/><name val="Calibri"/></font></fonts>'
        '<fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill></fills>'
        '<borders count="1"><border><left/><right/><top/><bottom/><diagonal/></border></borders>'
        '<cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs>'
        '<cellXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/></cellXfs></styleSheet>',
        "xl/workbook.xml": f'{XML}<workbook xmlns="{s}" xmlns:r="{ODOC}"><sheets>'
        '<sheet name="Groceries" sheetId="1" r:id="rId1"/></sheets></workbook>',
        "xl/worksheets/sheet1.xml": f'{XML}<worksheet xmlns="{s}"><sheetData>{"".join(xml_rows)}</sheetData></worksheet>',
    })


def pptx(path):
    p = "http://schemas.openxmlformats.org/presentationml/2006/main"
    a = "http://schemas.openxmlformats.org/drawingml/2006/main"
    ns = f'xmlns:a="{a}" xmlns:r="{ODOC}" xmlns:p="{p}"'
    emu = 914400

    def shape(i, name, x, y, w, h, text, size, bold=False, color="1F2937"):
        b = ' b="1"' if bold else ""
        return (f'<p:sp><p:nvSpPr><p:cNvPr id="{i}" name="{name}"/><p:cNvSpPr txBox="1"/><p:nvPr/></p:nvSpPr>'
                f'<p:spPr><a:xfrm><a:off x="{int(x * emu)}" y="{int(y * emu)}"/><a:ext cx="{int(w * emu)}" cy="{int(h * emu)}"/></a:xfrm>'
                '<a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr>'
                f'<p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US" sz="{size * 100}"{b}>'
                f'<a:solidFill><a:srgbClr val="{color}"/></a:solidFill></a:rPr><a:t>{text}</a:t></a:r></a:p></p:txBody></p:sp>')

    def slide(shapes):
        return (f'{XML}<p:sld {ns}><p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>'
                f'<p:grpSpPr/>{shapes}</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>')

    slides = [
        shape(2, "Title", 0.8, 2.2, 8.4, 1.2, "Nova Craft sample deck", 40, True, "4338CA")
        + shape(3, "Subtitle", 0.8, 3.5, 8.4, 0.8, "A PowerPoint file made for testing DeckCraft", 20),
        shape(2, "Title", 0.8, 0.6, 8.4, 1.0, "Second slide", 32, True)
        + shape(3, "Body", 0.8, 1.8, 8.4, 3.0, "Plain text on a white slide.", 20),
    ]
    layout_rel = rels(("rId1", f"{ODOC}/slideMaster", "../slideMasters/slideMaster1.xml"))
    master = (f'{XML}<p:sldMaster {ns}><p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>'
              '<p:grpSpPr/></p:spTree></p:cSld><p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" '
              'accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/>'
              '<p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst></p:sldMaster>')
    layout = (f'{XML}<p:sldLayout {ns} type="blank"><p:cSld name="Blank"><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/>'
              '<p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/></p:spTree></p:cSld></p:sldLayout>')
    theme_colors = "".join(f'<a:{n}><a:srgbClr val="{v}"/></a:{n}>' for n, v in [
        ("dk1", "000000"), ("lt1", "FFFFFF"), ("dk2", "1F2937"), ("lt2", "F3F4F6"), ("accent1", "4338CA"), ("accent2", "0EA5E9"),
        ("accent3", "10B981"), ("accent4", "F59E0B"), ("accent5", "EF4444"), ("accent6", "8B5CF6"), ("hlink", "2563EB"), ("folHlink", "7C3AED")])
    theme = (f'{XML}<a:theme xmlns:a="{a}" name="Nova"><a:themeElements><a:clrScheme name="Nova">{theme_colors}</a:clrScheme>'
             '<a:fontScheme name="Nova"><a:majorFont><a:latin typeface="Noto Sans"/><a:ea typeface=""/><a:cs typeface=""/></a:majorFont>'
             '<a:minorFont><a:latin typeface="Noto Sans"/><a:ea typeface=""/><a:cs typeface=""/></a:minorFont></a:fontScheme>'
             '<a:fmtScheme name="Nova"><a:fillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill>'
             '<a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:fillStyleLst>'
             '<a:lnStyleLst><a:ln w="9525"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln w="9525"><a:solidFill>'
             '<a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln w="9525"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln>'
             '</a:lnStyleLst><a:effectStyleLst><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle>'
             '<a:effectStyle><a:effectLst/></a:effectStyle></a:effectStyleLst><a:bgFillStyleLst><a:solidFill><a:schemeClr val="phClr"/>'
             '</a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill>'
             '</a:bgFillStyleLst></a:fmtScheme></a:themeElements></a:theme>')
    parts = {
        "[Content_Types].xml": f'{XML}<Types xmlns="{CT}">'
        '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
        '<Default Extension="xml" ContentType="application/xml"/>'
        '<Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>'
        '<Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/>'
        '<Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/>'
        '<Override PartName="/ppt/theme/theme1.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/>'
        + "".join(f'<Override PartName="/ppt/slides/slide{i}.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>'
                  for i in range(1, len(slides) + 1))
        + "</Types>",
        "_rels/.rels": rels(("rId1", f"{ODOC}/officeDocument", "ppt/presentation.xml")),
        "ppt/_rels/presentation.xml.rels": rels(("rId1", f"{ODOC}/slideMaster", "slideMasters/slideMaster1.xml"),
                                                ("rId2", f"{ODOC}/theme", "theme/theme1.xml"),
                                                *[(f"rId{i + 2}", f"{ODOC}/slide", f"slides/slide{i}.xml") for i in range(1, len(slides) + 1)]),
        "ppt/presentation.xml": f'{XML}<p:presentation {ns}><p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/></p:sldMasterIdLst>'
        '<p:sldIdLst>' + "".join(f'<p:sldId id="{255 + i}" r:id="rId{i + 2}"/>' for i in range(1, len(slides) + 1)) + '</p:sldIdLst>'
        f'<p:sldSz cx="{10 * emu}" cy="{int(5.625 * emu)}"/><p:notesSz cx="{int(5.625 * emu)}" cy="{10 * emu}"/></p:presentation>',
        "ppt/slideMasters/slideMaster1.xml": master,
        "ppt/slideMasters/_rels/slideMaster1.xml.rels": rels(("rId1", f"{ODOC}/slideLayout", "../slideLayouts/slideLayout1.xml"),
                                                             ("rId2", f"{ODOC}/theme", "../theme/theme1.xml")),
        "ppt/slideLayouts/slideLayout1.xml": layout,
        "ppt/slideLayouts/_rels/slideLayout1.xml.rels": layout_rel,
        "ppt/theme/theme1.xml": theme,
    }
    for i, shapes in enumerate(slides, 1):
        parts[f"ppt/slides/slide{i}.xml"] = slide(shapes)
        parts[f"ppt/slides/_rels/slide{i}.xml.rels"] = rels(("rId1", f"{ODOC}/slideLayout", "../slideLayouts/slideLayout1.xml"))
    package(path, parts)


def pdf(path):
    pages = [
        "BT /F1 28 Tf 72 740 Td (Nova Craft sample PDF) Tj ET\n"
        "BT /F1 14 Tf 72 700 Td (A PDF made for testing PdfCraft on Nova.) Tj ET\n"
        "0.26 0.22 0.79 rg 72 560 220 100 re f\n"
        "BT /F1 12 Tf 72 530 Td (Page 1 of 2) Tj ET",
        "BT /F1 28 Tf 72 740 Td (Second page) Tj ET\nBT /F1 12 Tf 72 530 Td (Page 2 of 2) Tj ET",
    ]
    objs = ["<< /Type /Catalog /Pages 2 0 R >>", None, "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>"]
    kids = []
    for content in pages:
        data = content.encode()
        objs.append(f"<< /Length {len(data)} >>\nstream\n{content}\nendstream")
        objs.append(f"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents {len(objs)} 0 R >>")
        kids.append(f"{len(objs)} 0 R")
    objs[1] = f"<< /Type /Pages /Kids [{' '.join(kids)}] /Count {len(kids)} >>"
    out = bytearray(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n")
    offsets = []
    for i, o in enumerate(objs, 1):
        offsets.append(len(out))
        out += f"{i} 0 obj\n{o}\nendobj\n".encode()
    xref = len(out)
    out += f"xref\n0 {len(objs) + 1}\n0000000000 65535 f \n".encode()
    out += "".join(f"{o:010d} 00000 n \n" for o in offsets).encode()
    out += f"trailer\n<< /Size {len(objs) + 1} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode()
    open(path, "wb").write(out)


def png(path, w=640, h=400):
    """A gradient with a few shapes, as an RGB PNG."""
    raw = bytearray()
    for y in range(h):
        raw.append(0)
        for x in range(w):
            r, g, b = 40 + 180 * x // w, 60 + 120 * y // h, 200 - 120 * x // w
            if (x - 200) ** 2 + (y - 200) ** 2 < 90 ** 2:
                r, g, b = 250, 200, 60
            elif 380 <= x < 560 and 120 <= y < 300:
                r, g, b = 240, 240, 245
            raw += bytes((r, g, b))

    def chunk(tag, d):
        return struct.pack(">I", len(d)) + tag + d + struct.pack(">I", zlib.crc32(tag + d) & 0xFFFFFFFF)

    data = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(bytes(raw), 9)) + chunk(b"IEND", b"")
    open(path, "wb").write(data)


def main():
    out = sys.argv[1]
    os.makedirs(out, exist_ok=True)
    docx(os.path.join(out, "sample.docx"))
    xlsx(os.path.join(out, "sample.xlsx"))
    pptx(os.path.join(out, "sample.pptx"))
    pdf(os.path.join(out, "sample.pdf"))
    png(os.path.join(out, "sample.png"))


if __name__ == "__main__":
    main()
