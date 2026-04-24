#pragma once
#include "sedsprintf.h"
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct {
  SedsRouter *r;
  uint8_t created;
  uint64_t start_time;
} RouterState;

extern RouterState g_router;

SedsResult tx_send(const uint8_t *bytes, size_t len, void *user);
SedsResult on_radio_packet(const SedsPacketView *pkt, void *user);

SedsResult init_telemetry_router(void);

SedsResult log_telemetry_synchronous(SedsDataType data_type, const void *data,
                                     size_t element_count, size_t element_size);
SedsResult log_telemetry_asynchronous(SedsDataType data_type, const void *data,
                                      size_t element_count, size_t element_size);
SedsResult log_telemetry_string_asynchronous(SedsDataType data_type, const char *str);

SedsResult dispatch_tx_queue(void);
SedsResult process_rx_queue(void);
SedsResult dispatch_tx_queue_timeout(uint32_t timeout_ms);
SedsResult process_rx_queue_timeout(uint32_t timeout_ms);
SedsResult process_all_queues_timeout(uint32_t timeout_ms);
SedsResult telemetry_periodic(uint32_t timeout_ms);

void rx_asynchronous(const uint8_t *bytes, size_t len);

SedsResult print_telemetry_error(int32_t error_code);
SedsResult log_error_asynchronous(const char *fmt, ...);
SedsResult log_error_synchronous(const char *fmt, ...);
SedsResult log_error_asyncronous(const char *fmt, ...);
SedsResult log_error_syncronous(const char *fmt, ...);

SedsResult telemetry_poll_timesync(void);
SedsResult telemetry_announce_discovery(void);
SedsResult telemetry_poll_discovery(void);

uint64_t telemetry_now_ms(void);
uint64_t telemetry_unix_ms(void);
uint64_t telemetry_unix_s(void);
uint8_t telemetry_unix_is_valid(void);

void telemetry_set_unix_time_ms(uint64_t unix_ms);
#ifdef __cplusplus
}
#endif
