// C# wrapper for unthinkable_search.dll (x64). Works on .NET Framework 4.x and
// .NET 6+. Put unthinkable_search.dll (and us.exe, for live updates without
// admin rights) next to your program, or call Engine.LoadFrom(path) first.
//
//   using (var e = UnthinkableSearch.Engine.Open(null, live: true))
//   {
//       e.WaitReady(30000);
//       foreach (var it in e.Search("*.pdf", UnthinkableSearch.Sort.Modified, descending: true, max: 50))
//           Console.WriteLine("{0}  {1:N0}  {2:u}", it.FullPath, it.Size, it.Modified);
//   }
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

namespace UnthinkableSearch
{
    public enum Sort : uint { Name = 0, Path = 1, Size = 2, Modified = 3, Created = 4 }

    public sealed class Item
    {
        public string FullPath;
        public string Name;
        public string Directory;
        public ulong Size;            // folders: recursive size
        public DateTime? Modified;    // UTC
        public DateTime? Created;     // UTC
        public uint Attributes;       // FILE_ATTRIBUTE_*
        public bool IsDirectory;
    }

    public static class Native
    {
        const string Dll = "unthinkable_search.dll";
        [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)] public static extern uint us_api_version();
        [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)] public static extern IntPtr us_version();
        [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)] public static extern UIntPtr us_last_error(byte[] buf, UIntPtr cap);
        [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)] public static extern IntPtr us_open(byte[] dataDir, uint flags);
        [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)] public static extern void us_close(IntPtr e);
        [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)] public static extern int us_wait_ready(IntPtr e, uint timeoutMs);
        [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)] public static extern int us_state(IntPtr e);
        [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)] public static extern int us_is_live(IntPtr e);
        [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)] public static extern UIntPtr us_status_text(IntPtr e, byte[] buf, UIntPtr cap);
        [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)] public static extern ulong us_generation(IntPtr e);
        [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)] public static extern ulong us_entry_count(IntPtr e);
        [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)] public static extern IntPtr us_search(IntPtr e, byte[] query, uint sort, int descending);
        [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)] public static extern void us_results_free(IntPtr r);
        [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)] public static extern uint us_results_count(IntPtr r);
        [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)] public static extern double us_results_ms(IntPtr r);
        [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)] public static extern UIntPtr us_results_full_path(IntPtr r, uint i, byte[] buf, UIntPtr cap);
        [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)] public static extern UIntPtr us_results_name(IntPtr r, uint i, byte[] buf, UIntPtr cap);
        [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)] public static extern UIntPtr us_results_dir(IntPtr r, uint i, byte[] buf, UIntPtr cap);
        [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)] public static extern ulong us_results_size(IntPtr r, uint i);
        [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)] public static extern ulong us_results_mtime(IntPtr r, uint i);
        [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)] public static extern ulong us_results_ctime(IntPtr r, uint i);
        [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)] public static extern uint us_results_attrs(IntPtr r, uint i);
        [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)] public static extern int us_results_is_dir(IntPtr r, uint i);
        [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)] public static extern IntPtr us_request(IntPtr e, byte[] json);
        [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)] public static extern void us_free_string(IntPtr s);

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        public static extern IntPtr LoadLibraryW(string path);

        public static byte[] Utf8Z(string s)
        {
            if (s == null) return null;
            var b = Encoding.UTF8.GetBytes(s);
            Array.Resize(ref b, b.Length + 1);
            return b;
        }

        public delegate UIntPtr Getter(byte[] buf, UIntPtr cap);

        // Two-call pattern: ask for the length, then fill an exact buffer.
        public static string GetString(Getter g)
        {
            int n = (int)g(null, UIntPtr.Zero);
            if (n == 0) return string.Empty;
            var buf = new byte[n + 1];
            g(buf, (UIntPtr)(uint)buf.Length);
            return Encoding.UTF8.GetString(buf, 0, n);
        }

        public static string PtrToUtf8(IntPtr p)
        {
            if (p == IntPtr.Zero) return null;
            int n = 0;
            while (Marshal.ReadByte(p, n) != 0) n++;
            var b = new byte[n];
            Marshal.Copy(p, b, 0, n);
            return Encoding.UTF8.GetString(b);
        }

        public static string LastError()
        {
            return GetString((b, c) => us_last_error(b, c));
        }
    }

    public sealed class Engine : IDisposable
    {
        IntPtr h;

        Engine(IntPtr handle) { h = handle; }

        /// Load the DLL from an explicit path (otherwise the normal DLL search applies).
        public static void LoadFrom(string dllPath)
        {
            if (Native.LoadLibraryW(dllPath) == IntPtr.Zero)
                throw new DllNotFoundException(dllPath + " (error " + Marshal.GetLastWin32Error() + ")");
        }

        /// dataDir: folder with index.usdb (null = next to the DLL).
        /// live: keep the index fresh (one UAC prompt if the process is not elevated).
        public static Engine Open(string dataDir, bool live)
        {
            var p = Native.us_open(Native.Utf8Z(dataDir), live ? 1u : 0u);
            if (p == IntPtr.Zero) throw new InvalidOperationException(Native.LastError());
            return new Engine(p);
        }

        public static string Version { get { return Native.PtrToUtf8(Native.us_version()); } }
        public bool WaitReady(int timeoutMs) { return Native.us_wait_ready(h, (uint)timeoutMs) != 0; }
        public bool IsLive { get { return Native.us_is_live(h) != 0; } }
        public ulong Generation { get { return Native.us_generation(h); } }
        public ulong EntryCount { get { return Native.us_entry_count(h); } }
        public string Status { get { return Native.GetString((b, c) => Native.us_status_text(h, b, c)); } }

        static DateTime? Ft(ulong ft) { return ft == 0 ? (DateTime?)null : DateTime.FromFileTimeUtc((long)ft); }

        /// Run a query and materialize up to `max` items (the full count is returned in `total`).
        public List<Item> Search(string query, Sort sort, bool descending, int max, out int total, out double ms)
        {
            var r = Native.us_search(h, Native.Utf8Z(query), (uint)sort, descending ? 1 : 0);
            if (r == IntPtr.Zero) throw new InvalidOperationException(Native.LastError());
            try
            {
                total = (int)Native.us_results_count(r);
                ms = Native.us_results_ms(r);
                var list = new List<Item>(Math.Min(total, max));
                for (uint i = 0; i < total && i < max; i++)
                {
                    uint k = i;
                    list.Add(new Item
                    {
                        FullPath = Native.GetString((b, c) => Native.us_results_full_path(r, k, b, c)),
                        Name = Native.GetString((b, c) => Native.us_results_name(r, k, b, c)),
                        Directory = Native.GetString((b, c) => Native.us_results_dir(r, k, b, c)),
                        Size = Native.us_results_size(r, k),
                        Modified = Ft(Native.us_results_mtime(r, k)),
                        Created = Ft(Native.us_results_ctime(r, k)),
                        Attributes = Native.us_results_attrs(r, k),
                        IsDirectory = Native.us_results_is_dir(r, k) != 0,
                    });
                }
                return list;
            }
            finally { Native.us_results_free(r); }
        }

        public List<Item> Search(string query, Sort sort = Sort.Name, bool descending = false, int max = 100)
        {
            int total; double ms;
            return Search(query, sort, descending, max, out total, out ms);
        }

        /// The `usc serve` JSON protocol in-process: {"cmd":"search","query":"..."}.
        public string Request(string json)
        {
            var p = Native.us_request(h, Native.Utf8Z(json));
            if (p == IntPtr.Zero) throw new InvalidOperationException(Native.LastError());
            try { return Native.PtrToUtf8(p); } finally { Native.us_free_string(p); }
        }

        public void Dispose()
        {
            if (h != IntPtr.Zero) { Native.us_close(h); h = IntPtr.Zero; }
        }
    }
}
