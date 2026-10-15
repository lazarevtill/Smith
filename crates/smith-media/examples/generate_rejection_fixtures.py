"""Generate synthetic animated containers using only fixture pixels and stdlib."""
from pathlib import Path
import struct
import zlib

out = Path(__file__).resolve().parents[1] / "tests" / "fixtures"


def png_chunk(kind, data):
    return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))


apng = b"\x89PNG\r\n\x1a\n"
apng += png_chunk(b"IHDR", struct.pack(">IIBBBBB", 1, 1, 8, 6, 0, 0, 0))
apng += png_chunk(b"acTL", struct.pack(">II", 2, 0))
apng += png_chunk(b"fcTL", struct.pack(">IIIIIHHBB", 0, 1, 1, 0, 0, 1, 10, 0, 0))
apng += png_chunk(b"IDAT", zlib.compress(b"\0\xff\0\0\xff"))
apng += png_chunk(b"fcTL", struct.pack(">IIIIIHHBB", 1, 1, 1, 0, 0, 1, 10, 0, 0))
apng += png_chunk(b"fdAT", struct.pack(">I", 2) + zlib.compress(b"\0\0\0\xff\xff"))
apng += png_chunk(b"IEND", b"")
(out / "animated.png").write_bytes(apng)


def riff_chunk(kind, data):
    return kind + struct.pack("<I", len(data)) + data + (b"\0" if len(data) % 2 else b"")


def u24(n):
    return n.to_bytes(3, "little")


# Reuse the generator's lossless 3x2 WebP pixel bitstream for two animation frames.
static = (out / "pixels.webp").read_bytes()
assert static[12:16] == b"VP8L"
chunks = riff_chunk(b"VP8X", bytes([2, 0, 0, 0]) + u24(2) + u24(1))
chunks += riff_chunk(b"ANIM", bytes(6))
frame = u24(0) + u24(0) + u24(2) + u24(1) + u24(100) + bytes([0]) + static[12:]
chunks += riff_chunk(b"ANMF", frame) + riff_chunk(b"ANMF", frame)
(out / "animated.webp").write_bytes(b"RIFF" + struct.pack("<I", 4 + len(chunks)) + b"WEBP" + chunks)
