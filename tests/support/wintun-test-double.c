/* User-mode API test double, not a network/kernel driver. MIT OR Apache-2.0. */
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#define API __declspec(dllexport)
typedef struct { DWORD unused; } ADAPTER;
typedef struct { HANDLE event; CRITICAL_SECTION lock; BYTE *packet; DWORD size; DWORD allocated; } SESSION;
static volatile LONG adapters;
static volatile LONG sessions;
static volatile LONG creates;

API PVOID WINAPI WintunCreateAdapter(PCWSTR name, PCWSTR type, const GUID *guid)
{ (void)name; (void)type; (void)guid; InterlockedIncrement(&creates); SetLastError(ERROR_ACCESS_DENIED); return NULL; }
API PVOID WINAPI WintunOpenAdapter(PCWSTR name)
{
    if (lstrcmpW(name, L"WinCAN mock") != 0) { SetLastError(ERROR_FILE_NOT_FOUND); return NULL; }
    ADAPTER *adapter = HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY, sizeof(*adapter));
    if (adapter) InterlockedIncrement(&adapters);
    return adapter;
}
API VOID WINAPI WintunCloseAdapter(PVOID adapter)
{ HeapFree(GetProcessHeap(), 0, adapter); InterlockedDecrement(&adapters); }
API VOID WINAPI WintunGetAdapterLUID(PVOID adapter, ULONGLONG *luid)
{ (void)adapter; *luid = 1; }
API DWORD WINAPI WintunGetRunningDriverVersion(VOID) { return 0x00010002; }
API PVOID WINAPI WintunStartSession(PVOID adapter, DWORD capacity)
{
    (void)adapter; (void)capacity;
    SESSION *session = HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY, sizeof(*session));
    if (!session) return NULL;
    session->event = CreateEventW(NULL, TRUE, FALSE, NULL);
    if (!session->event) { HeapFree(GetProcessHeap(), 0, session); return NULL; }
    InitializeCriticalSection(&session->lock); InterlockedIncrement(&sessions); return session;
}
API VOID WINAPI WintunEndSession(PVOID handle)
{
    SESSION *session = handle;
    if (session->packet) HeapFree(GetProcessHeap(), 0, session->packet);
    CloseHandle(session->event); DeleteCriticalSection(&session->lock);
    HeapFree(GetProcessHeap(), 0, session); InterlockedDecrement(&sessions);
}
API HANDLE WINAPI WintunGetReadWaitEvent(PVOID handle) { return ((SESSION *)handle)->event; }
API BYTE *WINAPI WintunReceivePacket(PVOID handle, DWORD *size)
{
    SESSION *session = handle; BYTE *packet;
    EnterCriticalSection(&session->lock); packet = session->packet;
    *size = session->size; session->packet = NULL; ResetEvent(session->event);
    LeaveCriticalSection(&session->lock);
    if (!packet) SetLastError(ERROR_NO_MORE_ITEMS);
    return packet;
}
API VOID WINAPI WintunReleaseReceivePacket(PVOID handle, const BYTE *packet)
{ (void)handle; HeapFree(GetProcessHeap(), 0, (PVOID)packet); }
API BYTE *WINAPI WintunAllocateSendPacket(PVOID handle, DWORD size)
{
    SESSION *session = handle; session->allocated = size;
    return HeapAlloc(GetProcessHeap(), 0, size);
}
API VOID WINAPI WintunSendPacket(PVOID handle, const BYTE *packet)
{
    SESSION *session = handle; EnterCriticalSection(&session->lock);
    if (session->packet) HeapFree(GetProcessHeap(), 0, session->packet);
    session->packet = (BYTE *)packet; session->size = session->allocated;
    SetEvent(session->event); LeaveCriticalSection(&session->lock);
}
API DWORD WINAPI WinCanTestLiveAdapters(VOID) { return (DWORD)adapters; }
API DWORD WINAPI WinCanTestLiveSessions(VOID) { return (DWORD)sessions; }
API DWORD WINAPI WinCanTestCreates(VOID) { return (DWORD)creates; }
