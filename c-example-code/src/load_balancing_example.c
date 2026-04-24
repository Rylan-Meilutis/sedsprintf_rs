#include "sedsprintf.h"
#include <stdint.h>
#include <stdio.h>

static SedsResult print_packet(const SedsPacketView *pkt, void *user)
{
    const char *label = (const char *)user;
    char buf[256];
    seds_pkt_to_string(pkt, buf, sizeof(buf));
    printf("[%s] %s\n", label, buf);
    return SEDS_OK;
}

int main(void)
{
    /* Assumes the runtime schema already contains GPS_DATA and RADIO. */
    SedsRouter *router = seds_router_new(Seds_RM_Sink, NULL, NULL, NULL, 0);
    int32_t side_a = seds_router_add_side_packet(router, "WAN_A", 5, print_packet, "router WAN_A", false);
    int32_t side_b = seds_router_add_side_packet(router, "WAN_B", 5, print_packet, "router WAN_B", false);
    seds_router_set_source_route_mode(router, -1, Seds_RSM_Weighted);
    seds_router_set_route_weight(router, -1, side_a, 3);
    seds_router_set_route_weight(router, -1, side_b, 1);

    printf("Configured weighted router split.\n");

    seds_router_free(router);
    return 0;
}
