"""unthinkable_search.dll from Python via ctypes (64-bit Python, stdlib only).

    python us_dll.py C:\\tools\\unthinkable-search\\unthinkable_search.dll "*.pdf"

API reference: crates/us-ffi/include/unthinkable_search.h, docs/INTEGRATION.md.
"""
import ctypes as C
import datetime as dt
import sys

SORT_NAME, SORT_PATH, SORT_SIZE, SORT_MTIME, SORT_CTIME = range(5)
OPEN_LIVE = 1


def load(path):
    d = C.CDLL(path)
    E, R = C.c_void_p, C.c_void_p
    sig = {
        "us_version": (C.c_char_p, []),
        "us_last_error": (C.c_size_t, [C.c_char_p, C.c_size_t]),
        "us_open": (E, [C.c_char_p, C.c_uint32]),
        "us_close": (None, [E]),
        "us_wait_ready": (C.c_int, [E, C.c_uint32]),
        "us_entry_count": (C.c_uint64, [E]),
        "us_search": (R, [E, C.c_char_p, C.c_uint32, C.c_int]),
        "us_results_free": (None, [R]),
        "us_results_count": (C.c_uint32, [R]),
        "us_results_ms": (C.c_double, [R]),
        "us_results_full_path": (C.c_size_t, [R, C.c_uint32, C.c_char_p, C.c_size_t]),
        "us_results_size": (C.c_uint64, [R, C.c_uint32]),
        "us_results_mtime": (C.c_uint64, [R, C.c_uint32]),
        "us_results_is_dir": (C.c_int, [R, C.c_uint32]),
        "us_request": (C.c_void_p, [E, C.c_char_p]),
        "us_free_string": (None, [C.c_void_p]),
    }
    for name, (res, args) in sig.items():
        f = getattr(d, name)
        f.restype, f.argtypes = res, args
    return d


def get_str(fn, *args):
    """Two-call pattern: query the length, then fill an exact buffer."""
    n = fn(*args, None, 0)
    buf = C.create_string_buffer(n + 1)
    fn(*args, buf, n + 1)
    return buf.value.decode("utf-8")


def filetime(ft):
    return None if ft == 0 else dt.datetime(1601, 1, 1, tzinfo=dt.timezone.utc) + dt.timedelta(microseconds=ft // 10)


def main(dll_path, query):
    us = load(dll_path)
    print("unthinkable_search", us.us_version().decode())
    eng = us.us_open(None, 0)  # None = <dll dir>\data; 0 = no live updates (no UAC)
    if not eng:
        raise RuntimeError(get_str(us.us_last_error))
    try:
        if not us.us_wait_ready(eng, 30000):
            raise RuntimeError("index not ready")
        r = us.us_search(eng, query.encode("utf-8"), SORT_MTIME, 1)
        if not r:
            raise RuntimeError(get_str(us.us_last_error))
        try:
            n = us.us_results_count(r)
            print(f"{n} results in {us.us_results_ms(r):.3f} ms (of {us.us_entry_count(eng)} entries)")
            for i in range(min(n, 20)):
                path = get_str(us.us_results_full_path, r, i)
                print(f"{us.us_results_size(r, i):>14,}  {filetime(us.us_results_mtime(r, i))}  {path}")
        finally:
            us.us_results_free(r)

        # Same thing through the JSON protocol:
        p = us.us_request(eng, b'{"cmd":"search","query":"notepad.exe","limit":3,"fields":["full","size"]}')
        try:
            print(C.string_at(p).decode("utf-8"))
        finally:
            us.us_free_string(p)
    finally:
        us.us_close(eng)


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "notepad.exe")
