# bin/_dock-tiles.py DOCK_PID — the Dock's tiles as the Dock itself lists
# them to accessibility clients, one line per tile:
#     tile<TAB>subrole<TAB>running (1|0)<TAB>title
# Kept, recent and running tiles alike, and a tile the Dock kept after
# its app was gone: no other source lists that one (it is in neither
# `defaults read com.apple.dock` nor `lsappinfo`).
#
# Prints `untrusted` and reads nothing when this process may not use
# accessibility: AXIsProcessTrusted only asks tccd whether access was
# granted, it never prompts.  Exits 2 when the Dock could not be read.
import ctypes, ctypes.util, sys

ax = ctypes.cdll.LoadLibrary(ctypes.util.find_library("ApplicationServices"))
cf = ctypes.cdll.LoadLibrary(ctypes.util.find_library("CoreFoundation"))
ref = ctypes.c_void_p
ax.AXIsProcessTrusted.restype = ctypes.c_bool
ax.AXUIElementCreateApplication.restype = ref
ax.AXUIElementCreateApplication.argtypes = [ctypes.c_int]
ax.AXUIElementCopyAttributeValue.restype = ctypes.c_int
ax.AXUIElementCopyAttributeValue.argtypes = [ref, ref, ctypes.POINTER(ref)]
cf.CFStringCreateWithCString.restype = ref
cf.CFStringCreateWithCString.argtypes = [ref, ctypes.c_char_p, ctypes.c_uint32]
cf.CFArrayGetCount.restype = ctypes.c_long
cf.CFArrayGetCount.argtypes = [ref]
cf.CFArrayGetValueAtIndex.restype = ref
cf.CFArrayGetValueAtIndex.argtypes = [ref, ctypes.c_long]
cf.CFGetTypeID.restype = ctypes.c_ulong
cf.CFGetTypeID.argtypes = [ref]
cf.CFStringGetTypeID.restype = ctypes.c_ulong
cf.CFBooleanGetTypeID.restype = ctypes.c_ulong
cf.CFArrayGetTypeID.restype = ctypes.c_ulong
cf.CFStringGetCString.restype = ctypes.c_bool
cf.CFStringGetCString.argtypes = [ref, ctypes.c_char_p, ctypes.c_long, ctypes.c_uint32]
cf.CFBooleanGetValue.restype = ctypes.c_bool
cf.CFBooleanGetValue.argtypes = [ref]
UTF8 = 0x08000100


def cfstr(s):
    return cf.CFStringCreateWithCString(None, s.encode(), UTF8)


def attr(elem, name):
    out = ref()
    if ax.AXUIElementCopyAttributeValue(elem, cfstr(name), ctypes.byref(out)) != 0:
        return None
    return out.value


def text(v):
    if not v or cf.CFGetTypeID(v) != cf.CFStringGetTypeID():
        return ""
    buf = ctypes.create_string_buffer(1024)
    return buf.value.decode(errors="replace") if cf.CFStringGetCString(v, buf, 1024, UTF8) else ""


def items(v):
    if not v or cf.CFGetTypeID(v) != cf.CFArrayGetTypeID():
        return None
    return [cf.CFArrayGetValueAtIndex(v, i) for i in range(cf.CFArrayGetCount(v))]


if not ax.AXIsProcessTrusted():
    print("untrusted")
    sys.exit(0)
lists = items(attr(ax.AXUIElementCreateApplication(int(sys.argv[1])), "AXChildren"))
if lists is None:
    print("the Dock listed no children", file=sys.stderr)
    sys.exit(2)
for lst in lists:
    for t in items(attr(lst, "AXChildren")) or []:
        run = attr(t, "AXIsApplicationRunning")
        running = bool(run) and cf.CFGetTypeID(run) == cf.CFBooleanGetTypeID() and cf.CFBooleanGetValue(run)
        title = text(attr(t, "AXTitle")).replace("\t", " ").replace("\n", " ")
        print("tile\t%s\t%d\t%s" % (text(attr(t, "AXSubrole")), running, title))
