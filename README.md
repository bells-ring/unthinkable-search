# unthinkable-search

**English** | [Русский](README.ru.md)

**The fastest file name search in the world.** A file name search for Windows (NTFS). It builds its index from the MFT and keeps it current with the USN journal.

* **0.4 ms** to find every file whose name contains `a` among 478,000 entries.
* **0.09 ms** for a typical name like `notepad.exe`. Most of that is thread wake-up.
* **0.3 ms** for `windows\system32` as a path query, without building a single path string.
* **20 ms** from launch to a searchable index.
* **Under 1 ns per entry** on dense queries, around 34 GB/s of filename text scanned per second on a desktop CPU.
* One 0.9 MB exe. No installer, service, registry keys, tray icon or network access.

To measure it on your own machine, see "How to check our numbers" below. If you find something faster, open an issue with your numbers.

## Numbers

Every figure here comes from `usc bench` and `usc bench-serve` on an index of 478,389 entries (405k files, 73k folders, about 12.8 MB of folded file names), run on a 6 thread x86-64 desktop CPU with AVX2. Times are medians of 60 rounds. Scan cost scales roughly with entry count divided by core count, so a 1 million entry index on the same CPU takes about twice as long.

| Query | Matches | In process (DLL, Rust) | Through `usc serve` (JSON, 100 full paths back) |
|---|---:|---:|---:|
| `notepad.exe` | 15 | 0.09 ms | 0.13 ms |
| `config` | 5,837 | 0.10 ms | 0.18 ms |
| `dll` | 42,511 | 0.17 ms | 0.27 ms |
| `microsoft` | 47,132 | 0.22 ms | 0.28 ms |
| `*.dll` | 26,273 | 0.26 ms | 0.34 ms |
| `windows\system32` (path) | 27,417 | 0.27 ms | 0.39 ms |
| `ows\sys` (path) | 40,744 | 0.28 ms | 0.38 ms |
| `dll\|exe` | 51,542 | 0.29 ms | 0.43 ms |
| `a` (59% of all entries) | 282,651 | 0.38 ms | 0.48 ms |
| `!e` | 150,399 | 0.48 ms | 0.64 ms |
| 22 mixed queries, sum of medians | | 5.2 ms | 7.2 ms |

Size-independent figures:

| What | Value |
|---|---|
| Throughput on dense queries | about 34 GB/s of name text, under 1 ns per entry |
| Time floor of any query | about 0.08 ms (thread wake-up and result hand-off) |
| Typing `notepad`, 7 keystrokes with narrowing | 1.4 ms in total |
| Index in memory | about 100 bytes per entry (46 MiB for 478k) |
| Whole process, index plus path tree plus buffers | 61 to 69 MiB for 478k, roughly 130 to 145 bytes per entry |
| Loading a saved index | about 2 GB/s (46 MB in 20 to 25 ms) |
| Building from scratch | 1.6 s for 520 MB of NTFS metadata, which is the speed of the disks, not of the CPU |
| CPU time to parse one MFT record | about 0.15 µs |
| Live update visible in results | 60 to 430 ms after the file changes on disk |

For comparison, the same engine measured 15.8 ms for the 22 queries before the path automaton, the dense AVX2 kernel and the allocator cache, and 5.45 ms after (2.9 times faster, same index, same process, interleaved runs).

## Download and run

Releases contain two archives:

| Archive | Contents |
|---|---|
| `unthinkable-search-<version>-windows-x64.zip` (about 1 MB) | `us.exe` (search window), `usc.exe` (command line and server). Unzip anywhere and start `us.exe`. You can rename the exe. |
| `unthinkable-search-sdk-<version>-windows-x64.zip` (about 1.5 MB) | `unthinkable_search.dll`, the C header, docs and examples for embedding. |

All files the program writes go into `data\` next to the exe. Checksums are in `SHA256SUMS.txt`.

```powershell
.\us.exe                        # search window; Ctrl+Alt+F shows or hides it
.\us.exe --live off             # no UAC prompt, no live updates
.\usc.exe index                 # build the index (run as admin to read the MFT)
.\usc.exe search "ext:pdf report" --format json --sort mtime --desc -n 20
.\usc.exe serve --pipe          # server for other programs: \\.\pipe\unthinkable-search
```

## Features

* **Search window.** Global hotkey Ctrl+Alt+F, results update as you type. Columns for name, path, size (recursive for folders), modified and created dates, sortable. The Explorer context menu (Shift for the extended one), Del, Shift+Del, Ctrl+C, Ctrl+X, Ctrl+Shift+C to copy the path, F2 to rename, Alt+Enter for properties.
* **Query language.** `foo bar` is AND, `a|b` is OR, `!x` is NOT, `*.pdf` and `a?c` are wildcards, `ext:pdf;docx` filters by extension, `file:` and `folder:` filter by type, `windows\system32` searches the full path. Case and diacritics are ignored (`é` matches `e`, `ё` matches `е`).
* **Live updates.** Create, rename, move and delete show up in results within a fraction of a second.
* **Embeddable.** `usc search --format json|jsonl|tsv|csv|nul`, a JSON lines server on stdin/stdout or a named pipe, a C ABI DLL with wrappers for C# and Python, and Rust crates. See [docs/INTEGRATION.md](docs/INTEGRATION.md) (currently in Russian).
* **Privilege separation.** Only a small helper process runs as administrator, once per session, and it sends metadata only. Pipes are limited to the current user and to the local machine. System DLLs load from System32 only.

## How it gets this fast

Here is every technique we use, grouped by where it acts. Most are small on their own; the speed is their sum.

### Reading the disk

1. **Read the `$MFT` directly.** On NTFS the master file table already holds every name, size and date. We open the volume and read it as one file, instead of asking Windows to list 70,000 folders one by one.
2. **Large aligned unbuffered reads.** Blocks are big and page aligned (`FILE_FLAG_NO_BUFFERING`), with several reads in flight, so the disk queue never runs empty.
3. **Parse while reading.** Worker threads parse blocks as they arrive. Parsing is so cheap (0.15 µs per record) that total time is the disk read time.
4. **Skip free records.** The `$MFT:$BITMAP` stream says which records are unused. We never read or parse those.
5. **One pass over every attribute.** Name and parent come from `$FILE_NAME`, dates and flags from `$STANDARD_INFORMATION`, size from `$DATA`, all in the same visit to the record. Fixups, extension records, hard links, DOS short names and orphans are handled in that single pass.
6. **Thread count by device type.** We ask Windows whether the volume has a seek penalty. A hard disk gets one reader (more threads only add seeks, 1.35 s against 1.5 to 1.9 s measured), an SSD gets two. Blocks from 256 KiB to 4 MiB measured the same, so the size is not tuned further.
7. **All volumes in parallel.** A slow HDD does not hold up the SSD.
8. **Fast fallback without admin rights.** `FindFirstFileEx` with `FindExInfoBasic` (no 8.3 short names) and `FIND_FIRST_EX_LARGE_FETCH` (bigger kernel buffer), a thread pool per volume, never descending into junctions and symlinks. Cloud placeholders are never opened, so nothing gets downloaded.

### Staying current

9. **Follow the USN journal.** Instead of rescanning, we read the change journal from a saved cursor. The cursor lives inside the index file, so after a restart only the missed delta is replayed.
10. **Poll cheaply, apply in batches.** The journal is polled without blocking and changes are collected with an adaptive delay, so a burst of changes costs one rebuild.
11. **Read the new state from the MFT, not from the file.** For a changed record we ask the filesystem for its MFT record (`FSCTL_GET_NTFS_FILE_RECORD`). The file is never opened, so there is no last-access update, no antivirus scan and no sharing conflict.
12. **Rebuild instead of patching.** A new sorted snapshot is built from the old index minus the changed records plus their new state, in about 60 ms for 500k entries. Renaming a folder fixes the path of every descendant for free, because paths are never stored. Searches keep running on the old snapshot until the new one is published.
13. **Safe fallbacks.** If the journal was reset or wrapped, only that volume is rescanned.

### How the data is laid out

14. **Structure of arrays.** Size, dates, attributes, parent and volume live in separate flat columns. A search touches only the columns it needs.
15. **One blob of folded names.** All names, lowercased and stripped of diacritics, sit back to back separated by `\0`. A substring search is one `memmem` over contiguous memory, with no pointer chasing and no per-name call.
16. **Sorted by name.** The blob is sorted, so every result list comes out already in name order. Sorting by name costs nothing.
17. **Delimiters turn patterns into substrings.** `foo*` becomes a search for `\0foo`, `*.pdf` and `ext:pdf` become `.pdf\0`, an exact name becomes `\0name\0`. These forms are exact, so no second check is needed.
18. **Parent links instead of paths.** An entry stores its parent id, not its path. The path is built only for rows you can see on screen. This saves memory and makes folder renames free.
19. **Cheap folding.** Case and diacritic folding has an ASCII fast path. All other text goes through Unicode decomposition once, at index time. A query is folded once, then compared byte for byte.
20. **Index build is parallel.** Names are folded in chunks on all cores, sorted by an 8 byte big endian prefix key with a full compare only on ties, and laid out in parallel.
21. **Lazy extras.** Folder sizes and the directory tree for path queries are computed once per index version, off the UI thread, before the snapshot is published.
22. **Load without copying.** The index file is read straight into the final vectors, with a word at a time checksum and full validation, so a damaged or tampered file is rejected instead of crashing a search.

### Searching

23. **SIMD substring search, in parallel.** The blob is cut into chunks at entry boundaries, up to eight chunks per thread so a skewed hit density does not leave cores idle. Concatenated chunk results stay sorted.
24. **Pick the most selective term.** In `foo bar`, the longer literal drives the scan and the other term is checked only on the candidates.
25. **Alternatives are scans, not checks.** `a|b` and `ext:jpg;png` run one scan per key and merge the sorted lists linearly.
26. **Galloping hit to entry mapping.** A hit is a byte offset. To find its entry we gallop forward from the previous entry, one to three probes, instead of bisecting the whole chunk, about fourteen probes.
27. **A dense AVX2 kernel.** For keys that occur very often (`a`, `e`), jumping between hits stops paying off. A second kernel reads 64 bytes per step, builds bit masks for the first byte, the last byte and the `\0` separators, and turns a hit into an entry number with a popcount. Once an entry matches, its remaining candidates in the same block are skipped. `a` went from 2.0 to 0.4 ms.
28. **Choose the kernel by measuring.** We sample eight 8 KiB windows spread over the blob and switch to the dense kernel only above 16,000 occurrences per MiB. Below that, memmem wins (a key like `re` at 6,000 per MiB is 15% slower on the dense kernel).
29. **Negation is a complement.** `!foo` runs the normal fast scan for `foo` and emits the entries between the hits, instead of testing every name.
30. **Path queries as a segment automaton.** This is the biggest single gain (2.3 ms to 0.3 ms). A path is a list of segments. The pattern `a\b\c` matches when a segment ends with `a`, the next one is exactly `b`, and the one after starts with `c`. We walk the folder tree once and store one byte per folder: "path already contains the pattern" plus "path ends with the first k parts". After that, every file is decided by the byte of its parent folder, plus one prefix compare in the rare case. No path string is ever built.
31. **Walk the tree in depth first order.** Folders are numbered so that every subtree is a contiguous range, which lets the walk run in parallel over subtrees, with small subtrees merged into batches.
32. **Folder names copied in walk order.** The names the automaton needs are stored in the same order it visits them, so it reads memory sequentially instead of jumping around a 13 MB blob. That alone made path queries about 30% faster. The state is 73 KB for 73,000 folders and stays in cache.
33. **Safe fallback for odd names.** If a name contains a backslash (possible in the NTFS POSIX namespace) or a pattern has more than 7 separators, the code falls back to matching the full path. Both paths are tested against a naive implementation.
34. **Tight loop for the common case.** A query of one path term skips the generic evaluator entirely. Name slices are fetched only when a term needs one.
35. **Narrow while typing.** When the new query only extends the previous one (`note` to `notep`), we filter the previous result instead of scanning again. Typing `notepad` costs 1.4 ms in total.
36. **Latest query wins.** A new keystroke cancels the running search. Chunks poll a cancel flag, so stale work stops within one chunk.

### Memory behavior

37. **Cache big blocks in the allocator.** On Windows, a freshly allocated buffer of a few hundred KiB gets new pages from the OS: a page fault and zeroing, about 0.8 µs per 4 KiB. A 1 MiB result buffer costs 0.2 ms before you write a single id. A small cache of freed blocks (32 KiB to 8 MiB, at most 8 MiB held) makes the same allocation cost 15 µs. It is released after every index publish so it never holds useless temporaries.
38. **Fill big results in parallel.** When the result is large, the threads write their parts into one pre-sized buffer together, instead of one thread copying all of it.
39. **Merge, don't sort.** Union of alternative scans is a linear merge of sorted lists.
40. **Compact ids.** Results are 32 bit entry ids. Names, sizes and dates are looked up only for the rows that are drawn.

### Interface and integration

41. **Virtual list.** The window uses an owner data list view. Windows asks only for the rows on screen, so showing 478,000 results costs the same as showing 30.
42. **Hotkey without hooks.** The window is created once and hidden. `RegisterHotKey` shows it, with no low level keyboard hook.
43. **Selection updates are coalesced.** Selecting a range fires thousands of notifications. We post one status refresh for all of them.
44. **Results are immutable snapshots.** Searches hold an `Arc` to the index version they started on. Live updates never block a search and never change its results mid flight.
45. **JSON written by hand.** The server and CLI write output straight into a reused buffer. The parser library runs only on incoming requests.
46. **Cheap embedding.** The DLL hands out result sets bound to an index snapshot and fills buffers you own, so a C# call that searches and reads ten rows costs 0.26 ms.

### Build

47. **Fat LTO, one codegen unit, `panic = abort`, opt level 3.** The release binary is 0.9 MB and has no runtime dependencies.
48. **No heavy dependencies.** The hot path uses `memchr` (SIMD) and `rayon` (work stealing). The rest is our own code.

### What we tried or considered and dropped

* **GPU search.** We never built it. A PCIe round trip is on the order of the whole CPU scan, so there is little to gain.
* **Compressing the hot parent column.** We skipped it on the strength of published experience from similar tools: a lot more CPU for almost no memory saved.
* **A system service.** It would keep an index always warm, but it needs an installer, registry entries and a SYSTEM process. A 20 ms start without one is good enough.
* **A delta overlay for live updates.** Rebuilding is 60 ms per batch at this size, so we shipped the simple version. It is still on the list for indexes beyond 10 million entries.
* **Using the dense kernel for all keys.** It loses on mid density keys, so it is chosen by sampling.

## How to check our numbers

```powershell
cargo build --release
.\target\release\usc.exe index           # needs admin to read the MFT
.\target\release\usc.exe bench           # in process, 22 queries, medians
.\target\release\usc.exe bench-serve     # through the JSON server, 100 rows per answer
```

Machines vary by 10 to 15% from run to run, so compare variants in one process, interleaved, as we did for every change in this file.

## Build

Rust 1.88 or newer (edition 2024). Tested with the `x86_64-pc-windows-gnu` toolchain. No C dependencies.

```powershell
cargo build --release                        # target\release\us.exe, usc.exe
cargo build --profile release-dll -p us-ffi  # target\release-dll\unthinkable_search.dll
cargo test --release --workspace
powershell -ExecutionPolicy Bypass -File tools\package.ps1   # release archives in dist\
```

## Repository layout

| Path | Contents |
|---|---|
| `crates/us-core` | index, folding, query parser, search (SIMD, AVX2 kernel, path automaton), sorting, MFT parser, live update logic, index file format, large block cache. Pure Rust, tests need no Windows. |
| `crates/us-win` | volume and `$MFT` reading, USN journal, directory walk, volume list. |
| `crates/us` | `us.exe` (window), `usc.exe` (CLI and server), index service, privilege helper, context menu. |
| `crates/us-ffi` | `unthinkable_search.dll` and the C header. |
| `docs/INTEGRATION.md` | embedding guide: CLI, server protocol, DLL, Rust. |
| `examples/` | C# and Python clients. |
| `tools/` | `package.ps1`, `serve-smoke.ps1` (server protocol), `dll-smoke.ps1` (DLL through C#). |

## Limits

* Windows 10 and 11 on x64. Fast indexing and live updates need NTFS. Other file systems are indexed by walking directories, without live updates.
* Reading the MFT needs one UAC prompt per session. Without it (`us.exe --live off`) the program uses the saved index or walks directories.
* Folder sizes count a file with several hard links once per name, so `C:\Windows` comes out larger than in Explorer.
* If live updates are off, changes reach the index only after a re-index.
* The hotkey is hard coded. There is no settings dialog yet.

## License

MIT, see [LICENSE](LICENSE).
