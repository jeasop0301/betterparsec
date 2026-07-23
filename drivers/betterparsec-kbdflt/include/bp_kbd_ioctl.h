/*
 * BetterParsec keyboard-filter private control protocol.
 * This is a test-signed prototype protocol, not a production ABI.
 */
#pragma once

#include <stdint.h>
#ifndef CTL_CODE
#include <winioctl.h>
#endif

#define BP_KBD_IOCTL_ARM \
    CTL_CODE(FILE_DEVICE_UNKNOWN, 0x800, METHOD_BUFFERED, FILE_ANY_ACCESS)
#define BP_KBD_IOCTL_DISARM \
    CTL_CODE(FILE_DEVICE_UNKNOWN, 0x801, METHOD_BUFFERED, FILE_ANY_ACCESS)
#define BP_KBD_IOCTL_READ_EVENTS \
    CTL_CODE(FILE_DEVICE_UNKNOWN, 0x802, METHOD_BUFFERED, FILE_ANY_ACCESS)
#define BP_KBD_IOCTL_STATUS \
    CTL_CODE(FILE_DEVICE_UNKNOWN, 0x803, METHOD_BUFFERED, FILE_ANY_ACCESS)

#define BP_KBD_PROTOCOL_VERSION 1u
#define BP_KBD_EVENT_KIND_KEY 1u

#pragma pack(push, 1)
typedef struct _BP_KBD_ARM_REQUEST {
    uint16_t version;
    uint16_t reserved0;
    uint32_t lease_ms;
    uint64_t nonce;
} BP_KBD_ARM_REQUEST;

typedef struct _BP_KBD_EVENT_V1 {
    uint16_t version;
    uint16_t kind;
    uint32_t sequence;
    uint64_t nonce;
    uint16_t make_code;
    uint16_t flags;
    uint32_t extra_information;
    uint64_t interrupt_time_100ns;
} BP_KBD_EVENT_V1;

typedef struct _BP_KBD_READ_EVENTS_HEADER {
    uint32_t count;
    uint32_t dropped;
} BP_KBD_READ_EVENTS_HEADER;

typedef struct _BP_KBD_STATUS {
    uint16_t version;
    uint16_t armed;
    uint32_t queued;
    uint32_t dropped;
    uint32_t reserved0;
    uint64_t nonce;
    uint64_t deadline_interrupt_time_100ns;
} BP_KBD_STATUS;
#pragma pack(pop)
