"""Synthetic independent ODF/BIFF8 fixtures using only Python stdlib; no office tools."""
import pathlib,struct,zipfile,zlib
p=pathlib.Path(__file__).parent
O='urn:oasis:names:tc:opendocument:xmlns:office:1.0';T='urn:oasis:names:tc:opendocument:xmlns:table:1.0';X='urn:oasis:names:tc:opendocument:xmlns:text:1.0';M='urn:oasis:names:tc:opendocument:xmlns:manifest:1.0'
content=f'''<office:document-content xmlns:office="{O}" xmlns:table="{T}" xmlns:text="{X}" office:version="1.2"><office:body><office:spreadsheet><table:table table:name="Данные"><table:table-row><table:table-cell office:value-type="string"><text:p>00123</text:p><text:p>Привет <text:s text:c="2"/>мир</text:p></table:table-cell><table:table-cell office:value-type="float" office:value="9007199254740993.00"><text:p>9007199254740993.00</text:p></table:table-cell></table:table-row><table:table-row table:number-rows-repeated="2"><table:table-cell office:value-type="boolean" office:boolean-value="true" table:number-columns-repeated="2"><text:p>TRUE</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>'''
manifest=f'<manifest:manifest xmlns:manifest="{M}"><manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.spreadsheet"/><manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/></manifest:manifest>'
with zipfile.ZipFile(p/'passive.ods','w',zipfile.ZIP_DEFLATED) as z:
 z.writestr('mimetype','application/vnd.oasis.opendocument.spreadsheet',compress_type=zipfile.ZIP_STORED)
 z.writestr('META-INF/manifest.xml',manifest);z.writestr('content.xml',content)
def rec(k,v=b''):return struct.pack('<HH',k,len(v))+v
def bof(t):return rec(0x809,struct.pack('<HHHHII',0x600,t,0xDBB,1997,0x41,6))
def s(s):return struct.pack('<HB',len(s),1)+s.encode('utf-16le')
strings=['00123','Привет'];sst=rec(0xfc,struct.pack('<II',2,2)+b''.join(map(s,strings)))
xf=rec(0xe0,bytes(9)+b'\x04'+bytes(10));xfdate=rec(0xe0,struct.pack('<HH',0,14)+bytes(5)+b'\x04'+bytes(10))
bound=lambda offset:rec(0x85,struct.pack('<IBBB',offset,0,0,4)+b'\x00Data')
global0=bof(5)+rec(0x42,struct.pack('<H',1200))+rec(0x22,b'\x00\x00')+xf+xfdate+sst
sheetoffset=len(global0)+len(bound(0))+4
global0+=bound(sheetoffset)+rec(0xa)
sheet=bof(0x10)+rec(0x200,struct.pack('<IIHHH',0,2,0,3,0))+rec(0xfd,struct.pack('<HHHI',0,0,0,0))+rec(0xfd,struct.pack('<HHHI',0,1,0,1))+rec(0x203,struct.pack('<HHHd',0,2,0,42.125))+rec(0x203,struct.pack('<HHHd',1,0,1,45292))+rec(0x205,struct.pack('<HHHBB',1,1,0,1,0))+rec(0x201,struct.pack('<HHH',1,2,0))+rec(0xa)
stream=global0+sheet
# 8 regular stream sectors, one directory, one FAT. No mini-stream needed.
END=0xfffffffe;FREE=0xffffffff
out=bytearray(11*512);out[:8]=bytes.fromhex('d0cf11e0a1b11ae1')
for a,v in [(24,0x3e),(26,3),(28,0xfffe),(30,9),(32,6)]:struct.pack_into('<H',out,a,v)
for a,v in [(44,1),(48,8),(56,4096),(60,END),(68,END)]:struct.pack_into('<I',out,a,v)
for i in range(109):struct.pack_into('<I',out,76+4*i,9 if i==0 else FREE)
out[512:512+len(stream)]=stream
for i,name,kind,start,length in [(0,'Root Entry',5,END,0),(1,'Workbook',2,0,4096)]:
 a=9*512+i*128;n=(name+'\0').encode('utf-16le');out[a:a+len(n)]=n;struct.pack_into('<HBBIII',out,a+64,len(n),kind,1,FREE,FREE,1 if i==0 else FREE);struct.pack_into('<IQ',out,a+116,start,length)
for i in range(128):struct.pack_into('<I',out,10*512+4*i,i+1 if i<7 else END if i<9 else 0xfffffffd if i==9 else FREE)
(p/'passive.xls').write_bytes(out)

# Deterministic synthetic image returned only by the loopback provider fixture.
def png_chunk(kind, data):
 return struct.pack('>I',len(data))+kind+data+struct.pack('>I',zlib.crc32(kind+data))
pixels=(b'\0'+bytes([40,80,120])*256)*256
png=b'\x89PNG\r\n\x1a\n'+png_chunk(b'IHDR',struct.pack('>IIBBBBB',256,256,8,2,0,0,0))+png_chunk(b'IDAT',zlib.compress(pixels))+png_chunk(b'IEND',b'')
(p/'provider-256.png').write_bytes(png)
