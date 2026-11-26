#!/usr/bin/env python3
"""_surface-png.py <iosurface id> <out.png>: the pixels of a global
IOSurface as written, BGRA with alpha ignored, as an RGB PNG.  A
window's own buffer, for a host whose locked screen gives no picture of
the window.  Standard library only."""
import ctypes
import struct
import sys
import zlib

io = ctypes.CDLL("/System/Library/Frameworks/IOSurface.framework/IOSurface")
cf = ctypes.CDLL("/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation")
io.IOSurfaceLookup.restype = ctypes.c_void_p
io.IOSurfaceLookup.argtypes = [ctypes.c_uint32]
io.IOSurfaceLock.argtypes = io.IOSurfaceUnlock.argtypes = [ctypes.c_void_p, ctypes.c_uint32, ctypes.c_void_p]
for f in (io.IOSurfaceGetWidth, io.IOSurfaceGetHeight, io.IOSurfaceGetBytesPerRow):
    f.restype, f.argtypes = ctypes.c_size_t, [ctypes.c_void_p]
io.IOSurfaceGetBaseAddress.restype = ctypes.c_void_p
io.IOSurfaceGetBaseAddress.argtypes = [ctypes.c_void_p]
cf.CFRelease.argtypes = [ctypes.c_void_p]
READ_ONLY = 1

sid, out = int(sys.argv[1]), sys.argv[2]
s = io.IOSurfaceLookup(sid)
if not s:
    sys.exit("no surface %d" % sid)
if io.IOSurfaceLock(s, READ_ONLY, None) != 0:
    cf.CFRelease(s)
    sys.exit("surface %d not locked" % sid)
w, h, bpr = io.IOSurfaceGetWidth(s), io.IOSurfaceGetHeight(s), io.IOSurfaceGetBytesPerRow(s)
raw = ctypes.string_at(io.IOSurfaceGetBaseAddress(s), bpr * h)
io.IOSurfaceUnlock(s, READ_ONLY, None)
cf.CFRelease(s)

rows = bytearray()
for y in range(h):
    bgra = raw[y * bpr:y * bpr + w * 4]
    rgb = bytearray(w * 3)
    rgb[0::3], rgb[1::3], rgb[2::3] = bgra[2::4], bgra[1::4], bgra[0::4]
    rows += b"\0" + rgb


def chunk(kind, data):
    return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))


with open(out, "wb") as f:
    f.write(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(bytes(rows), 6)) + chunk(b"IEND", b""))
print("surface %d: %dx%d" % (sid, w, h))
