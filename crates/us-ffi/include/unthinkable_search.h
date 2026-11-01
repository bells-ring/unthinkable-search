/*
 * unthinkable_search.h — C API of unthinkable_search.dll (x64 Windows).
 * Guide and examples: docs/INTEGRATION.md
 *
 * - All strings are UTF-8, NUL-terminated.
 * - Text getters copy into (buf, cap) and return the FULL length in bytes
 *   without the NUL; call with cap = 0 to get the required size.
 * - Every function is thread-safe. A results handle is an immutable snapshot:
 *   it stays valid and consistent while live updates publish newer indexes.
 * - The DLL keeps its index in <dll dir>\data (or the folder you pass) and
 *   launches <dll dir>\us.exe (or usc.exe) as the elevated broker when US_OPEN_LIVE is set
 *   and the host process is not elevated (one UAC prompt).
 */
#ifndef UNTHINKABLE_SEARCH_H
#define UNTHINKABLE_SEARCH_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#ifdef _WIN32
#  define US_API __declspec(dllimport)
#else
#  define US_API
#endif

#define US_API_VERSION 1

/* us_open flags */
#define US_OPEN_LIVE 1u /* keep the index fresh (USN journal) */

/* us_search sort keys */
#define US_SORT_NAME 0u
#define US_SORT_PATH 1u
#define US_SORT_SIZE 2u  /* folders: recursive size */
#define US_SORT_MTIME 3u /* date modified */
#define US_SORT_CTIME 4u /* date created */

/* us_state */
#define US_STATE_LOADING 0
#define US_STATE_INDEXING 1
#define US_STATE_READY 2

/* callback events */
#define US_EVENT_INDEX 1  /* new index snapshot (re-run your query) */
#define US_EVENT_STATUS 2 /* state / status text changed */

typedef struct us_engine us_engine;
typedef struct us_results us_results;
typedef void (*us_callback)(void *user, int event);

US_API uint32_t us_api_version(void);
US_API const char *us_version(void);
US_API size_t us_last_error(char *buf, size_t cap); /* per thread */

/* data_dir: NULL = <dll dir>\data. Returns NULL on error (see us_last_error). */
US_API us_engine *us_open(const char *data_dir, uint32_t flags);
US_API void us_close(us_engine *e); /* stops, saves the index, frees */
/* Called on the engine thread; keep it short, never call us_close from it. */
US_API void us_set_callback(us_engine *e, us_callback cb, void *user);

US_API int us_wait_ready(const us_engine *e, uint32_t timeout_ms); /* 1 ready, 0 timeout */
US_API int us_state(const us_engine *e);
US_API int us_is_live(const us_engine *e);
US_API size_t us_status_text(const us_engine *e, char *buf, size_t cap);
US_API uint64_t us_generation(const us_engine *e); /* bumps on every live update */
US_API uint64_t us_entry_count(const us_engine *e);

/* Query syntax: see docs/INTEGRATION.md ("foo bar", "a|b", "!x", "*.pdf",
 * "ext:pdf;docx", "file:", "folder:", "windows\system32").
 * Returns NULL if the index is not ready yet or on bad arguments. */
US_API us_results *us_search(const us_engine *e, const char *query, uint32_t sort, int descending);
US_API void us_results_free(us_results *r);
US_API uint32_t us_results_count(const us_results *r);
US_API double us_results_ms(const us_results *r);
US_API size_t us_results_full_path(const us_results *r, uint32_t i, char *buf, size_t cap);
US_API size_t us_results_name(const us_results *r, uint32_t i, char *buf, size_t cap);
US_API size_t us_results_dir(const us_results *r, uint32_t i, char *buf, size_t cap);
US_API uint64_t us_results_size(const us_results *r, uint32_t i);
US_API uint64_t us_results_mtime(const us_results *r, uint32_t i); /* FILETIME, 0 = unknown */
US_API uint64_t us_results_ctime(const us_results *r, uint32_t i); /* FILETIME, 0 = unknown */
US_API uint32_t us_results_attrs(const us_results *r, uint32_t i);  /* FILE_ATTRIBUTE_* */
US_API int us_results_is_dir(const us_results *r, uint32_t i);

/* JSON in, JSON out — the `usc serve` protocol (cmd: ping | info | search).
 * Free the result with us_free_string. */
US_API char *us_request(const us_engine *e, const char *json);
US_API void us_free_string(char *s);

#ifdef __cplusplus
}
#endif
#endif /* UNTHINKABLE_SEARCH_H */
