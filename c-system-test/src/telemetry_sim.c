#include "telemetry_sim.h"
#include <stdlib.h>
#include <stdio.h>

// ===================== BUS =====================

void bus_init(SimBus *bus) {
    bus->nodes = NULL;
    bus->count = 0;
    bus->cap = 0;
}

void bus_free(SimBus *bus) {
    free(bus->nodes);
    bus->nodes = NULL;
    bus->count = 0;
    bus->cap = 0;
}

size_t bus_register(SimBus *bus, SimNode *n) {
    if (bus->count == bus->cap) {
        size_t newcap = bus->cap ? bus->cap * 2 : 4;
        SimNode **tmp = (SimNode**)realloc(bus->nodes, newcap * sizeof(*tmp));
        if (!tmp) {
            fprintf(stderr, "bus_register: OOM\n");
            return (size_t)-1;
        }
        bus->nodes = tmp;
        bus->cap = newcap;
    }
    bus->nodes[bus->count] = n;
    return bus->count++;
}

SedsResult bus_send(SimBus *bus, const SimNode *from, const uint8_t *bytes, size_t len) {
    if (!bus || !bytes || !len) return SEDS_ERR;
    // Broadcast to all nodes except sender
    for (size_t i = 0; i < bus->count; ++i) {
        SimNode *n = bus->nodes[i];
        if (n == from) continue;
        node_rx(n, bytes, len);
    }
    return SEDS_OK;
}

// ===================== NODE =====================

// TX for a node: push raw bytes onto the bus.
// IMPORTANT: Only the *originator* of a packet is allowed to TX.
// Receivers never re-TX the same packet, preventing duplicates while
// preserving router local delivery order.
static SedsResult node_tx_send(const uint8_t *bytes, size_t len, void *user) {
    SimNode *self = (SimNode*)user;
    if (!self || !self->bus) return SEDS_ERR;

    if (!self->is_originator) {
        // Router asked a receiver to forward; ignore to prevent rebroadcast loops.
        return SEDS_OK;
    }
    return bus_send(self->bus, self, bytes, len);
}

SedsResult radio_handler(const SedsPacketView *pkt, void *user) {
    SimNode *self = user;
    char buf[seds_pkt_to_string_len(pkt)];
    SedsResult s = seds_pkt_to_string(pkt, buf, sizeof(buf));
    if (s != SEDS_OK) {
        fprintf(stderr, "[RADIO] to_string failed: %d\n", s);
        return s;
    }
    if (self) self->radio_hits++;
    printf("[%s:RADIO] %s\n", (self && self->name) ? self->name: "", buf);
    return SEDS_OK;
}

SedsResult sdcard_handler(const SedsPacketView *pkt, void *user) {
    SimNode *self = user;
    char buf[seds_pkt_to_string_len(pkt)];
    SedsResult s = seds_pkt_to_string(pkt, buf, sizeof(buf));
    if (s != SEDS_OK) {
        fprintf(stderr, "[SD] to_string failed: %d\n", s);
        return s;
    }
    if (self) self->sd_hits++;
    printf("[%s:SD] wrote: %s\n", (self && self->name) ? self->name : "SD", buf);
    return SEDS_OK;
}

SedsResult node_init(SimNode *n, SimBus *bus, const char *name, int radio, int sdcard) {
    if (!n || !bus) return SEDS_ERR;
    n->r = NULL;
    n->bus = bus;
    n->name = name ? name : "node";
    n->has_radio = radio ? 1 : 0;
    n->has_sdcard = sdcard ? 1 : 0;

    // Reset flags/stats
    n->is_originator = 0;
    n->in_rx = 0;
    n->radio_hits = 0;
    n->sd_hits = 0;

    // Local endpoint table
    SedsLocalEndpointDesc locals[2];
    uint32_t num = 0;

    if (n->has_radio) {
        locals[num++] = (SedsLocalEndpointDesc){
            .endpoint = SEDS_EP_RADIO,
            .handler  = radio_handler,
            .user     = (void*)n
        };
    }
    if (n->has_sdcard) {
        locals[num++] = (SedsLocalEndpointDesc){
            .endpoint = SEDS_EP_SD,   // <-- use your actual enum name
            .handler  = sdcard_handler,
            .user     = (void*)n
        };
    }

    // Construct router with node-specific TX and user ctx = this node
    n->r = seds_router_new(
        node_tx_send,
        n,                  // tx_user
        (num ? locals : NULL),
        num
    );
    if (!n->r) {
        fprintf(stderr, "[%s] Failed to create router\n", n->name);
        return SEDS_ERR;
    }

    // Register node on the bus
    bus_register(bus, n);
    return SEDS_OK;
}

void node_free(SimNode *n) {
    if (!n) return;
    if (n->r) {
        seds_router_free(n->r);
        n->r = NULL;
    }
    n->bus = NULL;
}

void node_rx(SimNode *n, const uint8_t *bytes, size_t len) {
    if (!n || !n->r || !bytes || !len) return;
    // Mark ingress only for debugging (not used by TX gate anymore)
    n->in_rx = 1;
    seds_router_receive(n->r, bytes, len);
    n->in_rx = 0;
}

SedsResult node_log(SimNode *n, SedsDataType dt, const float *data, size_t data_len) {
    if (!n || !n->r || !data || data_len == 0) return SEDS_ERR;
    const uint64_t ts = 0; // or a simulated clock
    n->is_originator = 1;  // allow TX
    SedsResult r = seds_router_log(n->r, dt, data, (uint32_t)data_len, ts);
    n->is_originator = 0;  // done
    return r;
}
