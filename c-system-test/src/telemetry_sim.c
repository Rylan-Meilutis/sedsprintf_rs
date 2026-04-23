// telemetry_sim_threadsafe.c
#include "telemetry_sim.h"
#include <stdlib.h>
#include <stdio.h>
#include <sys/time.h>
#include <stdint.h>
#include <string.h>

#ifdef _WIN32
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#else
#include <pthread.h>
#endif

/* ===================== Global recursive lock ===================== */

/* ===================== Time ===================== */

// Monotonic milliseconds (used by nodes + bus t0)
uint64_t host_now_ms(const void * user)
{
    (void) user;
#ifdef _WIN32
    static LARGE_INTEGER freq = {0};
    LARGE_INTEGER ctr;
    if (freq.QuadPart == 0) { QueryPerformanceFrequency(&freq); }
    QueryPerformanceCounter(&ctr);
    return (uint64_t) ((ctr.QuadPart * 1000ULL) / (uint64_t) freq.QuadPart);
#else
    struct timeval tv;
    gettimeofday(&tv, NULL);
    return (uint64_t) tv.tv_sec * 1000ULL + (uint64_t) (tv.tv_usec / 1000ULL);
#endif
}

/* ===================== BUS ===================== */

void bus_init(SimBus * bus)
{
    if (!bus) return;

    bus->nodes = NULL;
    bus->count = 0;
    bus->cap = 0;
    bus->t0_ms = host_now_ms(NULL); // zero point

    // NEW
    bus->relay = NULL;
    bus->relay_side_id = 0;
}


void bus_free(SimBus * bus)
{
    if (!bus) return;

    free(bus->nodes);
    bus->nodes = NULL;
    bus->count = 0;
    bus->cap = 0;
}

size_t bus_register(SimBus * bus, SimNode * n)
{
    if (!bus || !n) return (size_t) -1;


    if (bus->count == bus->cap)
    {
        size_t newcap = bus->cap ? bus->cap * 2 : 4;
        SimNode ** tmp = (SimNode **) realloc(bus->nodes, newcap * sizeof(*tmp));
        if (!tmp)
        {
            fprintf(stderr, "bus_register: OOM\n");

            return (size_t) -1;
        }
        bus->nodes = tmp;
        bus->cap = newcap;
    }
    bus->nodes[bus->count] = n;
    size_t id = bus->count++;

    return id;
}

SedsResult bus_send(SimBus * bus, const SimNode * from, const uint8_t * bytes, size_t len)
{
    if (!bus || !bytes || !len) return SEDS_ERR;

    // Broadcast to all nodes except sender
    for (size_t i = 0; i < bus->count; ++i)
    {
        SimNode * n = bus->nodes[i];
        if (n == from) continue;
        node_rx(n, bytes, len);
    }

    // Forward into relay only for locally-originated sends.
    if (bus->relay && from != NULL)
    {
        // Treat this as "incoming from this side"
        SedsResult r = seds_relay_rx_serialized_from_side(
            bus->relay,
            bus->relay_side_id,
            bytes,
            len
        );
        if (r != SEDS_OK)
        {
            fprintf(stderr, "bus_send: relay RX failed: %d\n", r);
        }
    }

    return SEDS_OK;
}


/* ===================== NODE ===================== */

static SedsResult ensure_endpoint(
    uint32_t id,
    const char * name,
    const char * description,
    bool link_local_only)
{
    SedsEndpointInfo info;
    SedsResult r = seds_endpoint_get_info_by_name(name, strlen(name), &info);
    if (r != SEDS_OK) return r;
    if (info.exists) return info.id == id ? SEDS_OK : SEDS_BAD_ARG;
    return seds_endpoint_register_ex(
        id,
        name,
        strlen(name),
        description,
        strlen(description),
        link_local_only);
}

static SedsResult ensure_dtype(
    uint32_t id,
    const char * name,
    const char * description,
    bool is_static,
    size_t element_count,
    uint8_t message_data_type,
    uint8_t message_class,
    uint8_t reliable,
    uint8_t priority,
    const uint32_t * endpoints,
    size_t num_endpoints)
{
    uint32_t endpoints_out[8];
    SedsDataTypeInfo info;
    SedsResult r = seds_dtype_get_info_by_name(
        name,
        strlen(name),
        endpoints_out,
        sizeof(endpoints_out) / sizeof(endpoints_out[0]),
        &info);
    if (r != SEDS_OK) return r;
    if (info.exists) return info.id == id ? SEDS_OK : SEDS_BAD_ARG;
    return seds_dtype_register_ex(
        id,
        name,
        strlen(name),
        description,
        strlen(description),
        is_static,
        element_count,
        message_data_type,
        message_class,
        reliable,
        priority,
        endpoints,
        num_endpoints);
}

static SedsResult register_test_schema(void)
{
    const uint32_t endpoints[] = {TEST_EP_RADIO, TEST_EP_SD_CARD};

    SedsResult r = ensure_endpoint(
        TEST_EP_SD_CARD,
        "SD_CARD",
        "On-board storage",
        false);
    if (r != SEDS_OK) return r;

    r = ensure_endpoint(
        TEST_EP_RADIO,
        "RADIO",
        "Radio link",
        false);
    if (r != SEDS_OK) return r;

    r = ensure_dtype(
        TEST_DT_GPS_DATA,
        "GPS_DATA",
        "GPS data",
        true,
        3,
        1, /* Float32 */
        0, /* Data */
        1, /* Ordered */
        80,
        endpoints,
        2);
    if (r != SEDS_OK) return r;

    r = ensure_dtype(
        TEST_DT_IMU_DATA,
        "IMU_DATA",
        "IMU data",
        true,
        6,
        1, /* Float32 */
        0, /* Data */
        0, /* None */
        40,
        endpoints,
        2);
    if (r != SEDS_OK) return r;

    r = ensure_dtype(
        TEST_DT_BATTERY_STATUS,
        "BATTERY_STATUS",
        "Battery status",
        true,
        2,
        1, /* Float32 */
        0, /* Data */
        0, /* None */
        60,
        endpoints,
        2);
    if (r != SEDS_OK) return r;

    r = ensure_dtype(
        TEST_DT_BAROMETER_DATA,
        "BAROMETER_DATA",
        "Barometer data",
        true,
        3,
        1, /* Float32 */
        0, /* Data */
        0, /* None */
        40,
        endpoints,
        2);
    if (r != SEDS_OK) return r;

    r = ensure_dtype(
        TEST_DT_MESSAGE_DATA,
        "MESSAGE_DATA",
        "Message data",
        false,
        0,
        13, /* String */
        0,  /* Data */
        0,  /* None */
        10,
        endpoints,
        2);
    if (r != SEDS_OK) return r;

    return ensure_dtype(
        TEST_DT_HEARTBEAT,
        "HEARTBEAT",
        "Heartbeat",
        true,
        0,
        15, /* NoData */
        0,  /* Data */
        0,  /* None */
        100,
        endpoints,
        2);
}

// TX for a node: push raw bytes onto the bus.
static SedsResult node_tx_send(const uint8_t * bytes, const size_t len, void * user)
{
    SimNode * self = (SimNode *) user;
    if (!self || !self->bus) return SEDS_ERR;
    // bus_send does its own locking
    return bus_send(self->bus, self, bytes, len);
}

SedsResult radio_handler_serial(const uint8_t * bytes, const size_t len, void * user)
{
    (void) len;
    SimNode * self = (SimNode *) user;

    // Deserialize into owned packet
    SedsOwnedPacket * owned = seds_pkt_deserialize_owned(bytes, len);
    if (!owned)
    {
        fprintf(stderr, "[RADIO] deserialize failed\n");
        return SEDS_ERR;
    }

    SedsPacketView view;
    if (seds_owned_pkt_view(owned, &view) != SEDS_OK)
    {
        fprintf(stderr, "[RADIO] owned_pkt_view failed\n");
        seds_owned_pkt_free(owned);
        return SEDS_ERR;
    }

    char buf[seds_pkt_to_string_len(&view)];
    SedsResult s = seds_pkt_to_string(&view, buf, sizeof(buf));
    if (s != SEDS_OK)
    {
        fprintf(stderr, "[RADIO] to_string failed: %d\n", s);
        seds_owned_pkt_free(owned);
        return s;
    }


    if (self) self->radio_hits++;
    printf("[RADIO] %s\n", buf);


    seds_owned_pkt_free(owned);
    return SEDS_OK;
}

SedsResult sdcard_handler(const SedsPacketView * pkt, void * user)
{
    SimNode * self = user;
    char buf[seds_pkt_to_string_len(pkt)];
    const SedsResult s = seds_pkt_to_string(pkt, buf, sizeof(buf));
    if (s != SEDS_OK)
    {
        fprintf(stderr, "[SD] to_string failed: %d\n", s);
        return s;
    }


    if (self) self->sd_hits++;
    printf("[SD] wrote: %s\n", buf);


    return SEDS_OK;
}

static uint64_t read_u64_le(const uint8_t * bytes)
{
    uint64_t v = 0;
    memcpy(&v, bytes, sizeof(v));
    return v;
}

uint64_t node_now_since_bus_ms(void * user)
{
    const SimNode * self = (const SimNode *) user;
    const uint64_t now = host_now_ms(NULL);
    // No need to lock for read-only snapshot here; t0_ms set at init and never changes
    return (self && self->bus) ? (now - self->bus->t0_ms) : 0;
}

SedsResult node_init(SimNode * n, SimBus * bus, const char * name, int radio, int sdcard, int time_source)
{
    if (!n || !bus) return SEDS_ERR;

    const SedsResult schema_result = register_test_schema();
    if (schema_result != SEDS_OK)
    {
        fprintf(stderr, "[%s] Failed to register test schema: %d\n", name ? name : "node", schema_result);
        return schema_result;
    }

    n->r = NULL;
    n->bus = bus;
    n->name = name ? name : "node";
    n->has_radio = radio ? 1 : 0;
    n->has_sdcard = sdcard ? 1 : 0;
    n->is_time_source = time_source ? 1 : 0;
    n->bus_side_id = 0;

    n->radio_hits = 0;
    n->sd_hits = 0;
    n->time_sync_hits = 0;
    n->ts_announce_hits = 0;
    n->ts_request_hits = 0;
    n->ts_response_hits = 0;

    SedsLocalEndpointDesc locals[2];
    uint32_t num = 0;

    if (n->has_radio)
    {
        locals[num++] = (SedsLocalEndpointDesc){
            .endpoint = TEST_EP_RADIO,
            .serialized_handler = radio_handler_serial,
            .user = (void *) n
        };
    }
    if (n->has_sdcard)
    {
        locals[num++] = (SedsLocalEndpointDesc){
            .endpoint = TEST_EP_SD_CARD, 
            .packet_handler = sdcard_handler,
            .user = (void *) n
        };
    }
    n->r = seds_router_new(
        Seds_RM_Sink,
        node_now_since_bus_ms,
        n, /* user */
        (num ? locals : NULL),
        num
    );
    if (!n->r)
    {
        fprintf(stderr, "[%s] Failed to create router\n", n->name);

        return SEDS_ERR;
    }
    if (seds_router_configure_timesync(
            n->r,
            true,
            n->is_time_source ? 1u : 0u,
            n->is_time_source ? 1u : 100u,
            5000u,
            100u,
            100u) != SEDS_OK)
    {
        fprintf(stderr, "[%s] Failed to configure router time sync\n", n->name);
        seds_router_free(n->r);
        n->r = NULL;
        return SEDS_ERR;
    }
    int32_t side_id = seds_router_add_side_serialized(n->r, "BUS", 3, node_tx_send, n, true);
    if (side_id < 0)
    {
        fprintf(stderr, "[%s] Failed to add router side\n", n->name);
        seds_router_free(n->r);
        n->r = NULL;
        return SEDS_ERR;
    }
    n->bus_side_id = (uint32_t) side_id;

    bus_register(bus, n);

    return SEDS_OK;
}

void node_free(SimNode * n)
{
    if (!n) return;

    if (n->r)
    {
        seds_router_free(n->r);
        n->r = NULL;
    }
    n->bus = NULL;
}

void node_rx(SimNode * n, const uint8_t * bytes, const size_t len)
{
    if (!n || !n->r || !bytes || !len) return;
    // LOCK();
    seds_router_rx_serialized_packet_to_queue_from_side(n->r, n->bus_side_id, bytes, len);
}

SedsResult node_log(
    SimNode * n,
    SedsDataType data_type,
    const void * data,
    size_t element_count,
    size_t element_size)
{
    if (!n || !n->r || !data) return SEDS_ERR;

    const size_t total_bytes = element_count * element_size;
    // LOCK();
    SedsResult r = seds_router_log_queue(n->r, data_type, data, total_bytes);

    return r;
}
