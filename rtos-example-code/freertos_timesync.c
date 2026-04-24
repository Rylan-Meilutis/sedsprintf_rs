#include "sedsprintf.h"
#include <stdint.h>
#include <stdio.h>

#include "FreeRTOS.h"
#include "task.h"

static uint64_t ticks_to_ms(TickType_t ticks)
{
    return ((uint64_t) ticks * 1000ULL) / (uint64_t) configTICK_RATE_HZ;
}

static uint64_t now_ms(void * user)
{
    (void) user;
    return ticks_to_ms(xTaskGetTickCount());
}

static SedsResult tx_send(const uint8_t * bytes, size_t len, void * user)
{
    (void) bytes;
    (void) len;
    (void) user;
    return SEDS_OK;
}

void timesync_task(void * arg)
{
    (void) arg;
    SedsRouter * r = seds_router_new(SEDS_RM_Sink, now_ms, NULL, NULL, 0);
    seds_router_add_side_serialized(r, "RADIO", 5, tx_send, NULL, true);
    seds_router_configure_timesync(r, true, 1U, 10U, 5000U, 1000U, 1000U);
    seds_router_set_local_network_datetime_millis(r, 2025, 1, 1, 12, 0, 0, 0);

    for (;;)
    {
        seds_router_periodic(r, 5);

        uint64_t network_ms = 0;
        if (seds_router_get_network_time_ms(r, &network_ms) == SEDS_OK)
        {
            printf("network_time_ms=%llu\n", (unsigned long long) network_ms);
        }

        vTaskDelay(pdMS_TO_TICKS(1000));
    }
}
