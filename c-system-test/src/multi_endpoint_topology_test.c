#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/time.h>
#include <unistd.h>

#include "sedsprintf.h"

enum {
    TEST_EP_SD_CARD = 100,
    TEST_EP_RADIO = 101
};

typedef struct
{
    unsigned packet_handler_hits;
    unsigned tx_packets;
    unsigned saw_radio_endpoint;
    unsigned saw_sdcard_endpoint;
} CaptureState;

static uint64_t host_now_ms(void *user)
{
    (void)user;
    struct timeval tv;
    gettimeofday(&tv, NULL);
    return (uint64_t)tv.tv_sec * 1000ULL + (uint64_t)(tv.tv_usec / 1000ULL);
}

static SedsResult noop_packet_handler(const SedsPacketView *pkt, void *user)
{
    (void)pkt;
    CaptureState *state = (CaptureState *)user;
    if (state != NULL)
    {
        state->packet_handler_hits++;
    }
    return SEDS_OK;
}

static SedsResult capture_tx(const uint8_t *bytes, size_t len, void *user)
{
    CaptureState *state = (CaptureState *)user;
    SedsOwnedPacket *owned = NULL;
    SedsPacketView view;
    uint32_t endpoints[16];
    int32_t got = 0;

    assert(state != NULL);
    assert(bytes != NULL);
    assert(len > 0U);

    state->tx_packets++;

    owned = seds_pkt_deserialize_owned(bytes, len);
    assert(owned != NULL);

    assert(seds_owned_pkt_view(owned, &view) == SEDS_OK);

    memset(endpoints, 0, sizeof(endpoints));
    got = seds_pkt_get_u32(&view, endpoints, sizeof(endpoints) / sizeof(endpoints[0]));
    if (got > 0)
    {
        for (int32_t i = 0; i < got; ++i)
        {
            if (endpoints[i] == (uint32_t)TEST_EP_RADIO)
            {
                state->saw_radio_endpoint = 1U;
            }
            if (endpoints[i] == (uint32_t)TEST_EP_SD_CARD)
            {
                state->saw_sdcard_endpoint = 1U;
            }
        }
    }

    seds_owned_pkt_free(owned);
    return SEDS_OK;
}

int main(void)
{
    CaptureState state = {0};
    bool did_queue = false;
    const SedsLocalEndpointDesc locals[] = {
        {
            .endpoint = TEST_EP_RADIO,
            .packet_handler = noop_packet_handler,
            .serialized_handler = NULL,
            .user = &state,
        },
        {
            .endpoint = TEST_EP_SD_CARD,
            .packet_handler = noop_packet_handler,
            .serialized_handler = NULL,
            .user = &state,
        },
    };

    SedsRouter *r = seds_router_new(
        Seds_RM_Relay,
        host_now_ms,
        NULL,
        locals,
        sizeof(locals) / sizeof(locals[0]));
    assert(r != NULL);

    assert(seds_router_add_side_serialized(r, "CAN", 3, capture_tx, &state, false) >= 0);
    assert(seds_router_configure_timesync(r, true, 0U, 0ULL, 5000ULL, 2000ULL, 2000ULL) ==
           SEDS_OK);

    assert(seds_router_announce_discovery(r) == SEDS_OK);
    assert(seds_router_process_tx_queue_with_timeout(r, 5U) == SEDS_OK);

    usleep(300000);

    assert(seds_router_poll_discovery(r, &did_queue) == SEDS_OK);
    assert(did_queue);
    assert(seds_router_process_tx_queue_with_timeout(r, 5U) == SEDS_OK);

    assert(state.tx_packets >= 2U);
    assert(state.saw_radio_endpoint != 0U);
    assert(state.saw_sdcard_endpoint != 0U);

    printf("multi-endpoint topology ok: tx=%u radio=%u sd=%u\n",
           state.tx_packets,
           state.saw_radio_endpoint,
           state.saw_sdcard_endpoint);

    seds_router_free(r);
    return 0;
}
