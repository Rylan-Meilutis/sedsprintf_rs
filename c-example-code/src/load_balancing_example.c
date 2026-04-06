#include "sedsprintf.h"
#include <stdbool.h>
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
    const SedsLocalEndpointDesc locals[] = {
        {.endpoint = SEDS_EP_RADIO, .packet_handler = NULL, .serialized_handler = NULL, .user = NULL},
    };

    SedsRouter *router = seds_router_new(Seds_RM_Sink, NULL, NULL, locals, 1);
    int32_t side_a = seds_router_add_side_packet(router, "WAN_A", 5, print_packet, "router WAN_A", false);
    int32_t side_b = seds_router_add_side_packet(router, "WAN_B", 5, print_packet, "router WAN_B", false);
    seds_router_set_source_route_mode(router, -1, Seds_RSM_Weighted);
    seds_router_set_route_weight(router, -1, side_a, 3);
    seds_router_set_route_weight(router, -1, side_b, 1);

    SedsRelay *relay = seds_relay_new(NULL, NULL);
    int32_t ingress = seds_relay_add_side_packet(relay, "INGRESS", 7, print_packet, "relay INGRESS", false);
    int32_t path_a = seds_relay_add_side_packet(relay, "PATH_A", 6, print_packet, "relay PATH_A", false);
    int32_t path_b = seds_relay_add_side_packet(relay, "PATH_B", 6, print_packet, "relay PATH_B", false);
    seds_relay_set_source_route_mode(relay, ingress, Seds_RSM_Failover);
    seds_relay_set_route_priority(relay, ingress, path_a, 0);
    seds_relay_set_route_priority(relay, ingress, path_b, 1);

    printf("Configured weighted router split and relay failover paths.\n");

    seds_relay_free(relay);
    seds_router_free(router);
    return 0;
}
