#include <stdio.h>
#include "telemetry_sim.h"
#include "sedsprintf.h"
#include <assert.h>
#include <stdio.h>
#include <unistd.h>
// Helper to generate some demo float samples
static void make_series(float * out, size_t n, float base)
{
    for (size_t i = 0; i < n; ++i) out[i] = base + (float) i * 0.25f;
}

int main(void)
{
    // 1) Create the bus
    SimBus bus;
    bus_init(&bus);

    // 2) Create three “boards”
    //    - A has RADIO handler (like our base example)
    //    - B has SD_CARD handler
    //    - C has no local endpoint (acts as producer/forwarder only)
    SimNode radioBoard, flightControllerBoard, powerBoard;
    assert(node_init(&radioBoard, &bus, "Radio Board", 1, 0) == SEDS_OK);
    assert(node_init(&flightControllerBoard, &bus, "Flight Controller Board", 0, 1) == SEDS_OK);
    assert(node_init(&powerBoard, &bus, "Power Board", 0, 0) == SEDS_OK);
    // 3) Send a variety of types from each board
    //    (Adjust SEDS_DT_* to match your definitions)
    float buf[8];

    // A logs GPS (3 floats)
    make_series(buf, 3, 10.0f);
    assert(node_log(&radioBoard, SEDS_DT_GPS, buf, 3) == SEDS_OK);

    // B logs IMU (6 floats)
    make_series(buf, 6, 0.5f);
    assert(node_log(&flightControllerBoard, SEDS_DT_IMU, buf, 6) == SEDS_OK);
    // C logs BATTERY (2 floats)
    make_series(buf, 4, 3.7f);
    assert(node_log(&powerBoard, SEDS_DT_BATTERY, buf, 4) == SEDS_OK);
    // B logs PRESSURE (1 float)
    make_series(buf, 3, 1013.25f);;
    assert(node_log(&flightControllerBoard, SEDS_DT_BAROMETER, buf, 3) == SEDS_OK);

    seds_router_process_tx_queue_with_timeout(flightControllerBoard.r, 100);
    seds_router_process_tx_queue_with_timeout(powerBoard.r, 100);
    seds_router_process_tx_queue_with_timeout(radioBoard.r, 100);


    seds_router_process_rx_queue_with_timeout(flightControllerBoard.r, 100);
    seds_router_process_rx_queue_with_timeout(powerBoard.r, 100);
    seds_router_process_rx_queue_with_timeout(radioBoard.r, 100);

    printf("A.radio_hits=%u, B.sd_hits=%u, C.radio_hits=%u, C.sd_hits=%u\n",
           radioBoard.radio_hits, flightControllerBoard.sd_hits, powerBoard.radio_hits, powerBoard.sd_hits);

    assert(radioBoard.radio_hits == 4);
    assert(flightControllerBoard.sd_hits == 4);
    assert(powerBoard.radio_hits == 0);
    assert(powerBoard.sd_hits == 0);
    // 4) Cleanup
    node_free(&radioBoard);
    node_free(&flightControllerBoard);
    node_free(&powerBoard);
    bus_free(&bus);

    return 0;
}
