#pragma once
#include "sedsprintf.h"
#include <stddef.h>
#include <stdint.h>

enum {
    TEST_DT_GPS_DATA = 100,
    TEST_DT_IMU_DATA = 101,
    TEST_DT_BATTERY_STATUS = 102,
    TEST_DT_BAROMETER_DATA = 104,
    TEST_DT_MESSAGE_DATA = 105,
    TEST_DT_HEARTBEAT = 106,
    TEST_EP_SD_CARD = 100,
    TEST_EP_RADIO = 101
};

#ifdef __cplusplus
extern "C" {



#endif

// Forward declare via typedef, then define the named struct.
typedef struct SimNode SimNode;

typedef struct
{
    SimNode ** nodes;
    size_t count;
    size_t cap;
    uint64_t t0_ms; // bus start time (ms)

    // NEW: optional relay linkage
    SedsRelay * relay;
    uint32_t relay_side_id;
} SimBus;

struct SimNode
{
    SedsRouter * r;
    const char * name;
    SimBus * bus;
    int has_radio;

    int has_sdcard;
    int is_time_source;
    uint32_t bus_side_id;
    // --- NEW: simple stats for tests ---
    unsigned radio_hits; // times RADIO handler ran
    unsigned sd_hits; // times SD handler ran
    unsigned time_sync_hits; // times TIME_SYNC handler ran
    unsigned ts_announce_hits; // TIME_SYNC_ANNOUNCE packets seen
    unsigned ts_request_hits; // TIME_SYNC_REQUEST packets seen
    unsigned ts_response_hits; // TIME_SYNC_RESPONSE packets seen
};

// Bus API unchanged...
void bus_init(SimBus * bus);

void bus_free(SimBus * bus);

size_t bus_register(SimBus * bus, SimNode * n);

SedsResult bus_send(SimBus * bus, const SimNode * from, const uint8_t * bytes, size_t len);

// Node API unchanged...
SedsResult node_init(SimNode * n, SimBus * bus, const char * name, int radio, int sdcard, int time_source);

void node_free(SimNode * n);

void node_rx(SimNode * n, const uint8_t * bytes, size_t len);

SedsResult node_log(
    SimNode * n,
    SedsDataType data_type,
    const void * data,
    size_t element_count,
    size_t element_size);

// Handlers
SedsResult radio_handler_serial(const uint8_t * bytes, size_t len, void * user);

SedsResult sdcard_handler(const SedsPacketView * pkt, void * user);
uint64_t host_now_ms(const void * user);

uint64_t node_now_since_bus_ms(void * user);
#ifdef __cplusplus
}
#endif
