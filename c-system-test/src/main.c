// main.c

#include <assert.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/time.h>
#include <unistd.h>

#include "telemetry_sim.h"
#include "sedsprintf.h"

#define num_endpoint_hits 40

// --------- helpers ----------
static void make_series(float * out, size_t n, float base)
{
    for (size_t i = 0; i < n; ++i) out[i] = base + (float) i * 0.25f;
}


// --------- relay TX helper (side -> bus) ----------
static SedsResult relay_side_tx_to_bus(const uint8_t * bytes, size_t len, void * user)
{
    SimBus * bus = (SimBus *) user;
    if (!bus) return SEDS_ERR;
    // from == NULL => deliver to all nodes on that bus
    return bus_send(bus, NULL, bytes, len);
}

static uint64_t relay_now_ms(void * user)
{
    (void) user;
    return host_now_ms(NULL);
}



static uint64_t gen_random_us(void)
{
    const uint32_t min_ms = 2, max_ms = 22;
    uint32_t v = 0;
    int fd = open("/dev/urandom", O_RDONLY);
    if (fd >= 0)
    {
        if (read(fd, &v, sizeof(v)) != sizeof(v)) v = 0;
        close(fd);
    }
    uint32_t ms = (v % (max_ms - min_ms + 1)) + min_ms;
    return (uint64_t) ms * 1000ULL;
}

// --------- global stop flag ----------
static volatile int g_stop = 0;

// --------- processor thread (per-node) ----------
static void * processor_thread(void * arg)
{
    SimNode * n = arg;
    while (!g_stop)
    {
        // Small timeouts so we can respond to g_stop quickly
        seds_router_process_tx_queue_with_timeout(n->r, 5);
        seds_router_process_rx_queue_with_timeout(n->r, 5);
        // Be nice to the scheduler
        usleep(1000);
    }
    // Final drain to avoid stragglers
    for (int i = 0; i < 50; ++i)
    {
        seds_router_process_tx_queue_with_timeout(n->r, 5);
        seds_router_process_rx_queue_with_timeout(n->r, 5);
        usleep(1000);
    }
    return NULL;
}

// --------- relay thread ----------
static void * relay_thread(void * arg)
{
    SedsRelay * relay = (SedsRelay *) arg;
    while (!g_stop)
    {
        // Small timeout so we can respond quickly to g_stop
        seds_relay_process_all_queues_with_timeout(relay, 5);
        usleep(1000);
    }
    // Final drain to avoid stragglers
    for (int i = 0; i < 50; ++i)
    {
        seds_relay_process_all_queues_with_timeout(relay, 5);
        usleep(1000);
    }
    return NULL;
}


// --------- sender threads (one per node) ----------
typedef struct
{
    SimNode * nodeA; // Radio Board (RADIO handler)
    SimNode * nodeB; // Flight Controller (SD handler)
    SimNode * nodeC; // Power Board (no local endpoints)
    SimNode * nodeD; // Valve board on a different bus (Radio handler)
} Topo;

static void * sender_A(void * arg)
{
    (void) arg;
    SimNode * A = ((Topo *) arg)->nodeA;
    float buf[8];
    for (int i = 0; i < 5; ++i)
    {
        make_series(buf, 3, 10.0f);
        assert(node_log(A, TEST_DT_GPS_DATA, buf, 3, sizeof(buf[0])) == SEDS_OK);
        usleep(gen_random_us());
    }
    return NULL;
}

static void * sender_B(void * arg)
{
    SimNode * B = ((Topo *) arg)->nodeB;
    float buf[8];
    for (int i = 0; i < 5; ++i)
    {
        make_series(buf, 6, 0.5f);
        assert(node_log(B, TEST_DT_IMU_DATA, buf, 6, sizeof(buf[0])) == SEDS_OK);
        usleep(gen_random_us());

        make_series(buf, 3, 101.3f);
        assert(node_log(B, TEST_DT_BAROMETER_DATA, buf, 3, sizeof(buf[0])) == SEDS_OK);
        usleep(gen_random_us());
        uint8_t buff[0];
        assert(node_log(B, TEST_DT_HEARTBEAT, buff, 0, 0) == SEDS_OK);
        usleep(gen_random_us());
    }
    return NULL;
}

static void * sender_C(void * arg)
{
    SimNode * C = ((Topo *) arg)->nodeC;
    float buf[8];
    for (int i = 0; i < 5; ++i)
    {
        make_series(buf, 2, 3.7f);
        assert(node_log(C, TEST_DT_BATTERY_STATUS, buf, 2, sizeof(buf[0])) == SEDS_OK);
        usleep(gen_random_us());

        const char * msg = "hello world!";
        assert(node_log(C, TEST_DT_MESSAGE_DATA, msg, strlen(msg), sizeof(msg[0])) == SEDS_OK);
        usleep(gen_random_us());
    }
    return NULL;
}


static void * sender_D(void * arg)
{
    SimNode * D = ((Topo *) arg)->nodeD;
    float buf[6];
    for (int i = 0; i < 5; ++i)
    {
        make_series(buf, 6, 3.5f);
        assert(node_log(D, TEST_DT_IMU_DATA, buf, 6, sizeof(buf[0])) == SEDS_OK);
        usleep(gen_random_us());

        const char * msg = "hello world from the valve board!";
        assert(node_log(D, TEST_DT_MESSAGE_DATA, msg, strlen(msg), sizeof(msg[0])) == SEDS_OK);
        usleep(gen_random_us());
    }
    return NULL;
}

int main(void)
{
    // 1) Bus + nodes
    SimBus bus1;
    bus_init(&bus1);
    SimBus bus2;
    bus_init(&bus2);

    // NEW: create relay and connect buses as sides
    SedsRelay * relay = seds_relay_new(relay_now_ms, NULL);
    assert(relay && "Failed to create relay");

    int32_t side_bus1 = seds_relay_add_side_serialized(
        relay,
        "bus1",
        4, // strlen("bus1")
        relay_side_tx_to_bus,
        &bus1,
        true
    );
    int32_t side_bus2 = seds_relay_add_side_serialized(
        relay,
        "bus2",
        4, // strlen("bus2")
        relay_side_tx_to_bus,
        &bus2,
        true
    );
    assert(side_bus1 >= 0 && side_bus2 >= 0);

    // Attach relay info to buses so bus_send can feed the relay
    bus1.relay = relay;
    bus1.relay_side_id = (uint32_t) side_bus1;
    bus2.relay = relay;
    bus2.relay_side_id = (uint32_t) side_bus2;

    SimNode radioBoard, flightControllerBoard, powerBoard, valve_board;
    assert(node_init(&radioBoard, &bus1, "Radio Board", 1, 0, 1) == SEDS_OK);
    assert(node_init(&flightControllerBoard, &bus1, "Flight Controller Board", 0, 1, 0) == SEDS_OK);
    assert(node_init(&powerBoard, &bus1, "Power Board", 0, 0, 0) == SEDS_OK);
    assert(node_init(&valve_board, &bus2, "Valve Board", 1, 0, 0) == SEDS_OK);

    // 2) Processor threads (one per node)
    pthread_t procA, procB, procC, procD, relay_th;
    pthread_create(&procA, NULL, processor_thread, &radioBoard);
    pthread_create(&procB, NULL, processor_thread, &flightControllerBoard);
    pthread_create(&procC, NULL, processor_thread, &powerBoard);
    pthread_create(&procD, NULL, processor_thread, &valve_board);

    // NEW: start relay thread
    pthread_create(&relay_th, NULL, relay_thread, relay);

    // 3) Sender threads (A, B, C, D)
    Topo topo = {
        .nodeA = &radioBoard,
        .nodeB = &flightControllerBoard,
        .nodeC = &powerBoard,
        .nodeD = &valve_board
    };
    pthread_t thA, thB, thC, thD;
    pthread_create(&thA, NULL, sender_A, &topo);
    pthread_create(&thB, NULL, sender_B, &topo);
    pthread_create(&thC, NULL, sender_C, &topo);
    pthread_create(&thD, NULL, sender_D, &topo);

    // 4) Wait for senders
    pthread_join(thA, NULL);
    pthread_join(thB, NULL);
    pthread_join(thC, NULL);
    pthread_join(thD, NULL);

    // 5) Wait until all asserted endpoint hits arrive or timeout.
    // This keeps the test from exiting early on slower machines while one
    // side is still draining its last forwarded packet.
    const uint64_t deadline_us = 15ULL * 1000 * 1000; // 15s safety
    struct timeval start, now;
    gettimeofday(&start, NULL);

    while (!(radioBoard.radio_hits == num_endpoint_hits
             && flightControllerBoard.sd_hits == num_endpoint_hits
             && valve_board.radio_hits == num_endpoint_hits))
    {
        gettimeofday(&now, NULL);
        uint64_t elapsed_us =
                (uint64_t) (now.tv_sec - start.tv_sec) * 1000000ULL +
                (uint64_t) (now.tv_usec - start.tv_usec);
        if (elapsed_us > deadline_us)
        {
            fprintf(stderr, "Timeout waiting for processors to drain queues\n");
            break;
        }
        usleep(1000);
    }

    // 6) Stop processors and join
    g_stop = 1;
    pthread_join(procA, NULL);
    pthread_join(procB, NULL);
    pthread_join(procC, NULL);
    pthread_join(procD, NULL);
    // NEW: stop relay thread too
    pthread_join(relay_th, NULL);

    printf("radioBoard.radio_hits=%u, radioBoard.sd_hits=%u, flightControllerBoard.radio_hits=%u, flightControllerBoard.sd_hits=%u, powerBoard.radio_hits=%u, powerBoard.sd_hits=%u valveBoard.radio_hits=%u, valveBoard.sd_hits=%u\n",
           radioBoard.radio_hits,radioBoard.sd_hits, flightControllerBoard.radio_hits, flightControllerBoard.sd_hits,
           powerBoard.radio_hits, powerBoard.sd_hits, valve_board.radio_hits, valve_board.sd_hits);
    printf("time_sync_hits: radio=%u, flight=%u, power=%u, valve=%u\n",
           radioBoard.time_sync_hits, flightControllerBoard.time_sync_hits,
           powerBoard.time_sync_hits, valve_board.time_sync_hits);

    uint64_t radio_network_ms = 0;
    uint64_t flight_network_ms = 0;
    const int radio_network_ok = seds_router_get_network_time_ms(radioBoard.r, &radio_network_ms);
    const int flight_network_ok = seds_router_get_network_time_ms(flightControllerBoard.r, &flight_network_ms);
    printf("network_time_ms status: radio=%d (%llu), flight=%d (%llu)\n",
           radio_network_ok, (unsigned long long) radio_network_ms,
           flight_network_ok, (unsigned long long) flight_network_ms);

    // 7) Assertions (may need adjusting depending on how many packets now cross the relay)
    assert(radioBoard.radio_hits == num_endpoint_hits);
    assert(radioBoard.sd_hits == 0);
    assert(flightControllerBoard.radio_hits == 0);
    assert(flightControllerBoard.sd_hits == num_endpoint_hits);
    assert(powerBoard.radio_hits == 0);
    assert(powerBoard.sd_hits == 0);
    assert(valve_board.radio_hits == num_endpoint_hits);
    assert(valve_board.sd_hits == 0);

    // 8) Cleanup
    node_free(&radioBoard);
    node_free(&flightControllerBoard);
    node_free(&powerBoard);
    node_free(&valve_board);

    bus_free(&bus2);
    bus_free(&bus1);

    // NEW: free relay
    seds_relay_free(relay);

    return 0;
}
