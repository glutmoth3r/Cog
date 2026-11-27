#ifndef COG_PLUGIN_V3_H
#define COG_PLUGIN_V3_H
#include <stddef.h>
#include <stdint.h>

#if defined(_WIN32)
#define COG_EXPORT __declspec(dllexport)
#else
#define COG_EXPORT __attribute__((visibility("default")))
#endif

#ifdef __cplusplus
extern "C" {
#endif

/* Trusted, version 3 ABI. Host value views are borrowed and callbacks run
 * on the VM thread. Async plugins may wake from worker threads, but may not
 * call any other host function from those threads. All views are borrowed.
 * The host table passed to an async start stays valid until that operation's
 * cancel or destroy_operation returns; wake_context has the same lifetime.
 * No exception/panic may cross C. See docs/SDK.md for the complete contract. */
/* Host-issued, invocation-scoped opaque token. The current host stores a
 * 52-bit generation in bits 63..12 and a zero-based 12-bit slot in bits 11..0.
 * Plugins must not inspect, synthesize, persist, or reuse handles. */
typedef uint64_t CogHandle;
typedef uint32_t CogStatus;
enum { COG_OK=0, COG_FAILURE=1, COG_INVALID=2, COG_TYPE_MISMATCH=3,
       COG_LIMIT=4, COG_PANIC=5, COG_PENDING=6, COG_ABI_VERSION=3 };
enum { COG_ANY=0, COG_INT=1, COG_STR=2, COG_BYTES=3, COG_HANDLE=4,
       COG_NIL=5, COG_NUMBER=6, COG_ARRAY=7, COG_RECORD=8, COG_FLOAT=9,
       COG_BOOL=10, COG_STRBUF=11, COG_ERROR=12, COG_CHAR=13, COG_UINT=14,
       COG_OPAQUE=255 };
enum { COG_OUTPUT=1, COG_INPUT=2, COG_FS_READ=4, COG_FS_WRITE=8,
       COG_PROCESS=16, COG_PROCESS_INFO=32, COG_CLOCK=64, COG_SLEEP=128,
       COG_ENTROPY=256, COG_NETWORK=512 };
typedef struct { const uint8_t *data; size_t len; } CogBytes;
typedef struct {
    uint32_t kind;
    uint64_t integer;
    double number;
    CogBytes bytes;
    const CogHandle *items;
    size_t len;
} CogValueView;
typedef struct {
    uint32_t size, version;
    CogStatus (*inspect)(void *, CogHandle, CogValueView *);
    CogStatus (*make)(void *, const CogValueView *, CogHandle *);
    CogStatus (*array_get)(void *, CogHandle, size_t, CogHandle *);
    CogStatus (*array_set)(void *, CogHandle, size_t, CogHandle);
    CogStatus (*write_output)(void *, CogBytes);
    CogStatus (*set_error)(void *, CogBytes);
    CogStatus (*wake)(void *);
} CogHostV3;
typedef CogStatus (*CogCallbackV3)(void *, const CogHostV3 *, void *,
                                  const CogHandle *, size_t, CogHandle *);
typedef CogStatus (*CogAsyncStartV3)(void *, const CogHostV3 *, void *, void *,
                                    const CogHandle *, size_t, void **);
typedef CogStatus (*CogAsyncPollV3)(void *, void *, const CogHostV3 *, void *,
                                   CogHandle *);
typedef void (*CogAsyncCancelV3)(void *, void *);
typedef void (*CogAsyncDestroyV3)(void *, void *);
typedef struct {
    uint32_t size;
    CogBytes name, parameters;
    uint8_t result;
    uint8_t asynchronous;
    uint16_t effects;
    uint64_t mutations;
    CogCallbackV3 invoke;
    CogAsyncStartV3 start;
    CogAsyncPollV3 poll;
    CogAsyncCancelV3 cancel;
    CogAsyncDestroyV3 destroy_operation;
} CogFunctionV3;
typedef struct {
    uint32_t size, version;
    size_t function_count;
    const CogFunctionV3 *functions;
    CogStatus (*create)(void **);
    void (*destroy)(void *);
} CogPluginV3;

COG_EXPORT const CogPluginV3 *cog_plugin_v3(void);

#ifdef __cplusplus
}
#endif
#endif
