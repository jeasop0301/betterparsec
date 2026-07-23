/*
 * BetterParsec keyboard class upper filter.
 * Test-signed prototype only: this is not production driver code.
 */

#include <ntddk.h>
#include <wdf.h>
#include <ntddkbd.h>
#include <kbdmou.h>
#include <wdmsec.h>
#include "bp_kbd_ioctl.h"

#define BP_RING_CAPACITY 128u
#define BP_100NS_PER_MILLISECOND 10000ull

typedef struct _BP_FILTER_CONTEXT {
    CONNECT_DATA UpperConnectData;
} BP_FILTER_CONTEXT, *PBP_FILTER_CONTEXT;

WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(BP_FILTER_CONTEXT, BpGetFilterContext);

typedef struct _BP_CAPTURE_STATE {
    KSPIN_LOCK Lock;
    BOOLEAN Armed;
    /*
     * A capture failure belongs to the nonce that observed it. A refresh is
     * not recovery: the broker must DISARM or choose a new nonce first.
     */
    BOOLEAN EpochFaulted;
    ULONGLONG Nonce;
    ULONGLONG Deadline;
    BOOLEAN LeftAltDown;
    BOOLEAN RightAltDown;
    /*
     * An Alt-down is held only until its next physical record. This lets the
     * filter decide whether it begins Alt+Tab without ever splitting a
     * non-Alt+Tab chord: the held record is replayed ahead of that record.
     */
    BOOLEAN PendingAlt;
    KEYBOARD_INPUT_DATA PendingAltInput;
    BOOLEAN CapturingAltTab;
    BOOLEAN AltTabDown;
    ULONG NextSequence;
    ULONG Head;
    ULONG Count;
    ULONG Dropped;
    BP_KBD_EVENT_V1 Events[BP_RING_CAPACITY];
} BP_CAPTURE_STATE;

static BP_CAPTURE_STATE gCapture;
static volatile LONG gCallbackBusy;

DRIVER_INITIALIZE DriverEntry;
EVT_WDF_DRIVER_DEVICE_ADD BpEvtDeviceAdd;
EVT_WDF_IO_QUEUE_IO_INTERNAL_DEVICE_CONTROL BpEvtInternalDeviceControl;
EVT_WDF_IO_QUEUE_IO_DEVICE_CONTROL BpEvtControlDeviceControl;

static VOID BpServiceCallback(
    _In_ PDEVICE_OBJECT DeviceObject,
    _In_ PKEYBOARD_INPUT_DATA InputDataStart,
    _In_ PKEYBOARD_INPUT_DATA InputDataEnd,
    _Inout_ PULONG InputDataConsumed);

static BOOLEAN
BpIsAlt(_In_ const KEYBOARD_INPUT_DATA *Input)
{
    return Input->MakeCode == 0x38;
}
static BOOLEAN
BpIsWin(_In_ const KEYBOARD_INPUT_DATA *Input)
{
    return Input->MakeCode == 0x5B || Input->MakeCode == 0x5C;
}
static BOOLEAN
BpIsChordModifier(_In_ const KEYBOARD_INPUT_DATA *Input)
{
    return Input->MakeCode == 0x1D || Input->MakeCode == 0x2A ||
        Input->MakeCode == 0x36;
}


static BOOLEAN
BpIsLocalSafetyKey(_In_ const KEYBOARD_INPUT_DATA *Input)
{
    return Input->MakeCode == 0x53 || /* Delete */
        Input->MakeCode == 0x10 ||    /* Q */
        Input->MakeCode == 0x29 ||    /* ` */
        Input->MakeCode == 0x3E;      /* F4 */
}



static VOID
BpUpdateAltStateLocked(_In_ const KEYBOARD_INPUT_DATA *Input)
{
    BOOLEAN down;

    if (!BpIsAlt(Input)) {
        return;
    }

    down = (Input->Flags & KEY_BREAK) == 0;
    if ((Input->Flags & KEY_E0) != 0) {
        gCapture.RightAltDown = down;
    } else {
        gCapture.LeftAltDown = down;
    }
}

static VOID
BpEnqueueEventLocked(
    _In_ const KEYBOARD_INPUT_DATA *Input,
    _In_ ULONGLONG Now)
{
    BP_KBD_EVENT_V1 *event =
        &gCapture.Events[(gCapture.Head + gCapture.Count) % BP_RING_CAPACITY];

    event->version = BP_KBD_PROTOCOL_VERSION;
    event->kind = BP_KBD_EVENT_KIND_KEY;
    event->sequence = ++gCapture.NextSequence;
    event->nonce = gCapture.Nonce;
    event->make_code = Input->MakeCode;
    event->flags = Input->Flags;
    event->extra_information = Input->ExtraInformation;
    event->interrupt_time_100ns = Now;
    ++gCapture.Count;
}

static VOID
BpFaultEpochLocked(VOID)
{
    gCapture.Armed = FALSE;
    gCapture.EpochFaulted = TRUE;
    gCapture.LeftAltDown = FALSE;
    gCapture.RightAltDown = FALSE;
    gCapture.CapturingAltTab = FALSE;
    gCapture.AltTabDown = FALSE;
    ++gCapture.Dropped;
}

static VOID
BpExpireLeaseLocked(_In_ ULONGLONG Now)
{
    if (gCapture.Armed && Now >= gCapture.Deadline) {
        BpFaultEpochLocked();
    }
}

static VOID
BpCallOriginal(
    _In_ PBP_FILTER_CONTEXT Context,
    _In_ PKEYBOARD_INPUT_DATA Start,
    _In_ PKEYBOARD_INPUT_DATA End,
    _Inout_ PULONG Consumed)
{
    PSERVICE_CALLBACK_ROUTINE service =
        (PSERVICE_CALLBACK_ROUTINE)Context->UpperConnectData.ClassService;

    if (service != NULL) {
        service(
            (PDEVICE_OBJECT)Context->UpperConnectData.ClassDeviceObject,
            Start,
            End,
            Consumed);
    } else {
        *Consumed = 0;
    }
}

static VOID
BpServiceCallback(
    _In_ PDEVICE_OBJECT DeviceObject,
    _In_ PKEYBOARD_INPUT_DATA InputDataStart,
    _In_ PKEYBOARD_INPUT_DATA InputDataEnd,
    _Inout_ PULONG InputDataConsumed)
{
    WDFDEVICE device = WdfWdmDeviceGetWdfDeviceHandle(DeviceObject);
    PBP_FILTER_CONTEXT context = BpGetFilterContext(device);
    ULONG index;
    ULONGLONG now;

    *InputDataConsumed = 0;
    if (InterlockedCompareExchange(&gCallbackBusy, 1, 0) != 0) {
        return;
    }
    for (index = 0; InputDataStart + index < InputDataEnd; ++index) {
        const KEYBOARD_INPUT_DATA *input = &InputDataStart[index];
        KEYBOARD_INPUT_DATA pendingAlt;
        KEYBOARD_INPUT_DATA priorPendingAlt;
        BOOLEAN armed;
        BOOLEAN replayPending = FALSE;
        BOOLEAN suppressLocal = FALSE;
        BOOLEAN keyDown = (input->Flags & KEY_BREAK) == 0;
        BOOLEAN wasAltDown;
        BOOLEAN isTab = input->MakeCode == 0x0F;
        BOOLEAN priorLeftAlt;
        BOOLEAN priorRightAlt;
        BOOLEAN priorPending;
        BOOLEAN priorCapturing;
        BOOLEAN priorTabDown;
        ULONGLONG nonce;
        KIRQL oldIrql;

        now = KeQueryInterruptTime();

        KeAcquireSpinLock(&gCapture.Lock, &oldIrql);
        BpExpireLeaseLocked(now);
        armed = gCapture.Armed;
        nonce = gCapture.Nonce;
        priorLeftAlt = gCapture.LeftAltDown;
        priorRightAlt = gCapture.RightAltDown;
        priorPending = gCapture.PendingAlt;
        priorPendingAlt = gCapture.PendingAltInput;
        priorCapturing = gCapture.CapturingAltTab;
        priorTabDown = gCapture.AltTabDown;
        wasAltDown = priorLeftAlt || priorRightAlt;
        if (!armed && gCapture.PendingAlt) {
            pendingAlt = gCapture.PendingAltInput;
            replayPending = TRUE;
        }
        if (armed) {
            if (gCapture.Count == BP_RING_CAPACITY) {
                if (gCapture.PendingAlt) {
                    pendingAlt = gCapture.PendingAltInput;
                    replayPending = TRUE;
                }
                BpFaultEpochLocked();
                armed = FALSE;
            } else {
                BpUpdateAltStateLocked(input);

                if (BpIsWin(input)) {
                    suppressLocal = TRUE;
                    if (gCapture.PendingAlt) {
                        gCapture.PendingAlt = FALSE;
                        gCapture.CapturingAltTab = TRUE;
                        gCapture.AltTabDown = FALSE;
                    }
                } else if (gCapture.CapturingAltTab) {
                    if (BpIsAlt(input) || isTab) {
                        suppressLocal = TRUE;
                        if (isTab) {
                            gCapture.AltTabDown = keyDown;
                        }
                        if (!keyDown && BpIsAlt(input) &&
                            !gCapture.LeftAltDown && !gCapture.RightAltDown &&
                            !gCapture.AltTabDown) {
                            gCapture.CapturingAltTab = FALSE;
                        }
                        if (!keyDown && isTab &&
                            !gCapture.LeftAltDown && !gCapture.RightAltDown) {
                            gCapture.CapturingAltTab = FALSE;
                        }
                    }
                } else if (gCapture.PendingAlt) {
                    if (isTab && keyDown) {
                        gCapture.PendingAlt = FALSE;
                        gCapture.CapturingAltTab = TRUE;
                        gCapture.AltTabDown = TRUE;
                        suppressLocal = TRUE;
                    } else if (!BpIsChordModifier(input)) {
                        pendingAlt = gCapture.PendingAltInput;
                        replayPending = TRUE;
                    }
                } else if (BpIsAlt(input) && keyDown && !wasAltDown) {
                    gCapture.PendingAltInput = *input;
                    gCapture.PendingAlt = TRUE;
                    suppressLocal = TRUE;
                }
            }
            if (armed && suppressLocal) {
                BpEnqueueEventLocked(input, now);
            }
        }
        KeReleaseSpinLock(&gCapture.Lock, oldIrql);

        if (!armed) {
            if (replayPending) {
                ULONG consumed = 0;
                BpCallOriginal(context, &pendingAlt, &pendingAlt + 1, &consumed);
                if (consumed == 0) {
                    goto Exit;
                }
                KeAcquireSpinLock(&gCapture.Lock, &oldIrql);
                gCapture.PendingAlt = FALSE;
                KeReleaseSpinLock(&gCapture.Lock, oldIrql);
            }
            {
                ULONG consumed = 0;
                BpCallOriginal(context, (PKEYBOARD_INPUT_DATA)input,
                    (PKEYBOARD_INPUT_DATA)input + 1, &consumed);
                if (consumed == 0) {
                    goto Exit;
                }
                ++*InputDataConsumed;
            }
            continue;
        }

        if (replayPending) {
            ULONG consumed = 0;
            BpCallOriginal(context, &pendingAlt, &pendingAlt + 1, &consumed);
            if (consumed == 0) {
                KeAcquireSpinLock(&gCapture.Lock, &oldIrql);
                if (gCapture.Armed && !gCapture.EpochFaulted &&
                    gCapture.Nonce == nonce) {
                    gCapture.LeftAltDown = priorLeftAlt;
                    gCapture.RightAltDown = priorRightAlt;
                    gCapture.PendingAlt = priorPending;
                    gCapture.PendingAltInput = priorPendingAlt;
                    gCapture.CapturingAltTab = priorCapturing;
                    gCapture.AltTabDown = priorTabDown;
                }
                KeReleaseSpinLock(&gCapture.Lock, oldIrql);
                goto Exit;
            }
            KeAcquireSpinLock(&gCapture.Lock, &oldIrql);
            gCapture.PendingAlt = FALSE;
            KeReleaseSpinLock(&gCapture.Lock, oldIrql);
            priorPending = FALSE;
        }

        if (!suppressLocal || BpIsLocalSafetyKey(input)) {
            ULONG consumed = 0;
            BpCallOriginal(context, (PKEYBOARD_INPUT_DATA)input,
                (PKEYBOARD_INPUT_DATA)input + 1, &consumed);
            if (consumed == 0) {
                KeAcquireSpinLock(&gCapture.Lock, &oldIrql);
                if (gCapture.Armed && !gCapture.EpochFaulted &&
                    gCapture.Nonce == nonce) {
                    gCapture.LeftAltDown = priorLeftAlt;
                    gCapture.RightAltDown = priorRightAlt;
                    gCapture.PendingAlt = priorPending;
                    gCapture.PendingAltInput = priorPendingAlt;
                    gCapture.CapturingAltTab = priorCapturing;
                    gCapture.AltTabDown = priorTabDown;
                }
                KeReleaseSpinLock(&gCapture.Lock, oldIrql);
                goto Exit;
            }
            if (!suppressLocal) {
                KeAcquireSpinLock(&gCapture.Lock, &oldIrql);
                if (gCapture.Armed && !gCapture.EpochFaulted &&
                    gCapture.Nonce == nonce) {
                    BpEnqueueEventLocked(input, now);
                }
                KeReleaseSpinLock(&gCapture.Lock, oldIrql);
            }
        }
        ++*InputDataConsumed;
    }

Exit:
    InterlockedExchange(&gCallbackBusy, 0);
}

static VOID
BpForwardInternalRequest(_In_ WDFDEVICE Device, _In_ WDFREQUEST Request)
{
    WdfRequestFormatRequestUsingCurrentType(Request);
    if (!WdfRequestSend(Request, WdfDeviceGetIoTarget(Device), WDF_NO_SEND_OPTIONS)) {
        WdfRequestComplete(Request, WdfRequestGetStatus(Request));
    }
}

VOID
BpEvtInternalDeviceControl(
    _In_ WDFQUEUE Queue,
    _In_ WDFREQUEST Request,
    _In_ size_t OutputBufferLength,
    _In_ size_t InputBufferLength,
    _In_ ULONG IoControlCode)
{
    WDFDEVICE device = WdfIoQueueGetDevice(Queue);
    PBP_FILTER_CONTEXT context = BpGetFilterContext(device);
    UNREFERENCED_PARAMETER(OutputBufferLength);
    UNREFERENCED_PARAMETER(InputBufferLength);

    if (IoControlCode == IOCTL_INTERNAL_KEYBOARD_CONNECT) {
        CONNECT_DATA *connectData;
        size_t length;
        NTSTATUS status = WdfRequestRetrieveInputBuffer(
            Request, sizeof(*connectData), (PVOID *)&connectData, &length);

        if (!NT_SUCCESS(status)) {
            WdfRequestComplete(Request, status);
            return;
        }
        if (context->UpperConnectData.ClassService != NULL) {
            WdfRequestComplete(Request, STATUS_SHARING_VIOLATION);
            return;
        }

        context->UpperConnectData = *connectData;
        connectData->ClassDeviceObject = WdfDeviceWdmGetDeviceObject(device);
        connectData->ClassService = (PVOID)BpServiceCallback;
    }

    BpForwardInternalRequest(device, Request);
}

static VOID
BpCompleteStatus(_In_ WDFREQUEST Request)
{
    BP_KBD_STATUS *status;
    size_t length;
    KIRQL oldIrql;
    ULONGLONG now = KeQueryInterruptTime();
    NTSTATUS ntstatus = WdfRequestRetrieveOutputBuffer(Request, sizeof(*status), (PVOID *)&status, &length);

    if (!NT_SUCCESS(ntstatus)) {
        WdfRequestComplete(Request, ntstatus);
        return;
    }

    KeAcquireSpinLock(&gCapture.Lock, &oldIrql);
    BpExpireLeaseLocked(now);
    status->version = BP_KBD_PROTOCOL_VERSION;
    status->armed = gCapture.Armed ? 1u : 0u;
    status->queued = gCapture.Count;
    status->dropped = gCapture.Dropped;
    status->reserved0 = 0;
    status->nonce = gCapture.Nonce;
    status->deadline_interrupt_time_100ns = gCapture.Deadline;
    KeReleaseSpinLock(&gCapture.Lock, oldIrql);
    WdfRequestCompleteWithInformation(Request, STATUS_SUCCESS, sizeof(*status));
}

VOID
BpEvtControlDeviceControl(
    _In_ WDFQUEUE Queue,
    _In_ WDFREQUEST Request,
    _In_ size_t OutputBufferLength,
    _In_ size_t InputBufferLength,
    _In_ ULONG IoControlCode)
{
    KIRQL oldIrql;
    UNREFERENCED_PARAMETER(Queue);
    UNREFERENCED_PARAMETER(OutputBufferLength);
    UNREFERENCED_PARAMETER(InputBufferLength);

    switch (IoControlCode) {
    case BP_KBD_IOCTL_ARM:
    {
        BP_KBD_ARM_REQUEST *arm;
        size_t length;
        ULONGLONG now;
        NTSTATUS status = WdfRequestRetrieveInputBuffer(
            Request, sizeof(*arm), (PVOID *)&arm, &length);
        if (!NT_SUCCESS(status)) {
            WdfRequestComplete(Request, status);
            return;
        }
        if (arm->version != BP_KBD_PROTOCOL_VERSION ||
            arm->lease_ms == 0 || arm->lease_ms > 1000) {
            WdfRequestComplete(Request, STATUS_INVALID_PARAMETER);
            return;
        }

        now = KeQueryInterruptTime();
        KeAcquireSpinLock(&gCapture.Lock, &oldIrql);
        BpExpireLeaseLocked(now);
        if (!gCapture.Armed && gCapture.PendingAlt) {
            KeReleaseSpinLock(&gCapture.Lock, oldIrql);
            WdfRequestComplete(Request, STATUS_DEVICE_BUSY);
            return;
        }
        if (gCapture.EpochFaulted && gCapture.Nonce == arm->nonce) {
            KeReleaseSpinLock(&gCapture.Lock, oldIrql);
            WdfRequestComplete(Request, STATUS_DEVICE_NOT_READY);
            return;
        }
        if (!gCapture.Armed || gCapture.Nonce != arm->nonce) {
            gCapture.Head = 0;
            gCapture.Count = 0;
            gCapture.Dropped = 0;
            gCapture.NextSequence = 0;
            gCapture.LeftAltDown = FALSE;
            gCapture.RightAltDown = FALSE;
            gCapture.CapturingAltTab = FALSE;
            gCapture.AltTabDown = FALSE;
            gCapture.EpochFaulted = FALSE;
        }
        gCapture.Armed = TRUE;
        gCapture.Nonce = arm->nonce;
        gCapture.Deadline = now +
            ((ULONGLONG)arm->lease_ms * BP_100NS_PER_MILLISECOND);
        KeReleaseSpinLock(&gCapture.Lock, oldIrql);
        WdfRequestComplete(Request, STATUS_SUCCESS);
        return;
    }

    case BP_KBD_IOCTL_DISARM:
        /*
         * Acquire the callback gate as an exclusive teardown fence. A callback
         * that is already inside kbdclass makes DISARM retryable; while value 2
         * is held, new callbacks report zero consumption and are retried by the
         * keyboard stack after the epoch is disarmed.
         */
        if (InterlockedCompareExchange(&gCallbackBusy, 2, 0) != 0) {
            WdfRequestComplete(Request, STATUS_DEVICE_BUSY);
            return;
        }
        KeAcquireSpinLock(&gCapture.Lock, &oldIrql);
        gCapture.Armed = FALSE;
        gCapture.EpochFaulted = FALSE;
        gCapture.LeftAltDown = FALSE;
        gCapture.RightAltDown = FALSE;
        gCapture.CapturingAltTab = FALSE;
        gCapture.AltTabDown = FALSE;
        KeReleaseSpinLock(&gCapture.Lock, oldIrql);
        InterlockedExchange(&gCallbackBusy, 0);
        WdfRequestComplete(Request, STATUS_SUCCESS);
        return;

    case BP_KBD_IOCTL_STATUS:
        BpCompleteStatus(Request);
        return;

    case BP_KBD_IOCTL_READ_EVENTS:
    {
        BP_KBD_READ_EVENTS_HEADER *header;
        BP_KBD_EVENT_V1 *events;
        size_t length;
        ULONG capacity;
        ULONG count;
        ULONG index;
        NTSTATUS status = WdfRequestRetrieveOutputBuffer(
            Request, sizeof(*header), (PVOID *)&header, &length);
        if (!NT_SUCCESS(status)) {
            WdfRequestComplete(Request, status);
            return;
        }

        capacity = (ULONG)((length - sizeof(*header)) / sizeof(BP_KBD_EVENT_V1));
        KeAcquireSpinLock(&gCapture.Lock, &oldIrql);
        if (InterlockedCompareExchange(&gCallbackBusy, 0, 0) != 0) {
            count = 0;
        } else {
            count = gCapture.Count < capacity ? gCapture.Count : capacity;
        }
        header->count = count;
        header->dropped = gCapture.Dropped;
        events = (BP_KBD_EVENT_V1 *)(header + 1);
        for (index = 0; index < count; ++index) {
            events[index] = gCapture.Events[(gCapture.Head + index) % BP_RING_CAPACITY];
        }
        gCapture.Head = (gCapture.Head + count) % BP_RING_CAPACITY;
        gCapture.Count -= count;
        if (count != 0) {
            gCapture.Dropped = 0;
        }
        KeReleaseSpinLock(&gCapture.Lock, oldIrql);
        WdfRequestCompleteWithInformation(
            Request, STATUS_SUCCESS, sizeof(*header) + count * sizeof(*events));
        return;
    }

    default:
        WdfRequestComplete(Request, STATUS_INVALID_DEVICE_REQUEST);
        return;
    }
}

static NTSTATUS
BpCreateControlDevice(_In_ WDFDRIVER Driver)
{
    PWDFDEVICE_INIT init;
    WDFDEVICE device;
    WDF_IO_QUEUE_CONFIG queueConfig;
    UNICODE_STRING name;
    UNICODE_STRING link;
    NTSTATUS status;

    init = WdfControlDeviceInitAllocate(Driver, &SDDL_DEVOBJ_SYS_ALL_ADM_ALL);
    if (init == NULL) {
        return STATUS_INSUFFICIENT_RESOURCES;
    }

    RtlInitUnicodeString(&name, L"\\Device\\BetterParsecKbd");
    status = WdfDeviceInitAssignName(init, &name);
    if (!NT_SUCCESS(status)) {
        WdfDeviceInitFree(init);
        return status;
    }
    WdfDeviceInitSetDeviceType(init, FILE_DEVICE_UNKNOWN);
    WdfDeviceInitSetCharacteristics(init, FILE_DEVICE_SECURE_OPEN, TRUE);

    status = WdfDeviceCreate(&init, WDF_NO_OBJECT_ATTRIBUTES, &device);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    WDF_IO_QUEUE_CONFIG_INIT_DEFAULT_QUEUE(&queueConfig, WdfIoQueueDispatchSequential);
    queueConfig.EvtIoDeviceControl = BpEvtControlDeviceControl;
    status = WdfIoQueueCreate(device, &queueConfig, WDF_NO_OBJECT_ATTRIBUTES, NULL);
    if (!NT_SUCCESS(status)) {
        WdfObjectDelete(device);
        return status;
    }

    RtlInitUnicodeString(&link, L"\\DosDevices\\BetterParsecKbd");
    status = WdfDeviceCreateSymbolicLink(device, &link);
    if (!NT_SUCCESS(status)) {
        WdfObjectDelete(device);
        return status;
    }

    WdfControlFinishInitializing(device);
    return STATUS_SUCCESS;
}

NTSTATUS
BpEvtDeviceAdd(_In_ WDFDRIVER Driver, _Inout_ PWDFDEVICE_INIT DeviceInit)
{
    WDF_OBJECT_ATTRIBUTES attributes;
    WDF_IO_QUEUE_CONFIG queueConfig;
    WDFDEVICE device;
    NTSTATUS status;
    UNREFERENCED_PARAMETER(Driver);

    WdfFdoInitSetFilter(DeviceInit);
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, BP_FILTER_CONTEXT);
    status = WdfDeviceCreate(&DeviceInit, &attributes, &device);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    WDF_IO_QUEUE_CONFIG_INIT_DEFAULT_QUEUE(&queueConfig, WdfIoQueueDispatchSequential);
    queueConfig.EvtIoInternalDeviceControl = BpEvtInternalDeviceControl;
    return WdfIoQueueCreate(device, &queueConfig, WDF_NO_OBJECT_ATTRIBUTES, NULL);
}

NTSTATUS
DriverEntry(_In_ PDRIVER_OBJECT DriverObject, _In_ PUNICODE_STRING RegistryPath)
{
    WDF_DRIVER_CONFIG config;
    WDFDRIVER driver;
    NTSTATUS status;

    KeInitializeSpinLock(&gCapture.Lock);
    WDF_DRIVER_CONFIG_INIT(&config, BpEvtDeviceAdd);
    config.DriverPoolTag = 'dKbB';
    status = WdfDriverCreate(DriverObject, RegistryPath, WDF_NO_OBJECT_ATTRIBUTES, &config, &driver);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    return BpCreateControlDevice(driver);
}
