#include "telemetry.h"
#include "sedsprintf.h"

#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/time.h>
#include <time.h>
#include <stdarg.h>

static uint8_t g_local_unix_valid = 0U;
static uint64_t g_local_unix_ms = 0ULL;

static uint64_t host_now_ms(void *user) {
  (void)user;
  struct timeval tv;
  gettimeofday(&tv, NULL);
  return (uint64_t)tv.tv_sec * 1000ULL + (uint64_t)(tv.tv_usec / 1000ULL);
}

RouterState g_router = {.r = NULL, .created = 0U, .start_time = 0ULL};

SedsResult tx_send(const uint8_t *bytes, size_t len, void *user) {
  (void)bytes;
  (void)len;
  (void)user;
  return SEDS_OK;
}

void rx_asynchronous(const uint8_t *bytes, size_t len) {
  if (!bytes || len == 0U) {
    return;
  }
  if (!g_router.r && init_telemetry_router() != SEDS_OK) {
    return;
  }
  (void)seds_router_rx_serialized_packet_to_queue(g_router.r, bytes, len);
}

SedsResult on_radio_packet(const SedsPacketView *pkt, void *user) {
  (void)user;
  char buf[seds_pkt_to_string_len(pkt)];
  SedsResult s = seds_pkt_to_string(pkt, buf, sizeof(buf));
  if (s != SEDS_OK) {
    printf("on_radio_packet: seds_pkt_to_string failed: %d\n", (int)s);
    return s;
  }
  printf("on_radio_packet: %s\n", buf);
  return SEDS_OK;
}

static int is_leap_year(int32_t year) {
  return ((year % 4) == 0 && (year % 100) != 0) || ((year % 400) == 0);
}

static uint64_t days_before_year(int32_t year) {
  uint64_t days = 0;
  for (int32_t y = 1970; y < year; ++y) {
    days += is_leap_year(y) ? 366ULL : 365ULL;
  }
  return days;
}

static uint64_t unix_ms_from_utc(int32_t year, uint8_t month, uint8_t day,
                                 uint8_t hour, uint8_t minute, uint8_t second,
                                 uint16_t millisecond) {
  static const uint16_t days_before_month[12] = {0U, 31U, 59U, 90U, 120U, 151U,
                                                 181U, 212U, 243U, 273U, 304U, 334U};
  uint64_t days = days_before_year(year) + days_before_month[month - 1U] + (uint64_t)(day - 1U);
  if (month > 2U && is_leap_year(year)) {
    days += 1ULL;
  }
  return (((days * 24ULL + hour) * 60ULL + minute) * 60ULL + second) * 1000ULL + millisecond;
}

static SedsResult apply_local_unix_to_router(SedsRouter *router) {
  static const uint8_t days_in_month[12] = {31U, 28U, 31U, 30U, 31U, 30U,
                                            31U, 31U, 30U, 31U, 30U, 31U};
  uint64_t whole_seconds;
  uint64_t days;
  uint32_t seconds_of_day;
  int32_t year = 1970;
  uint8_t month = 1U;
  uint8_t day = 1U;
  uint8_t hour;
  uint8_t minute;
  uint8_t second;
  uint16_t millisecond;

  if (!router || !g_local_unix_valid) {
    return SEDS_OK;
  }

  whole_seconds = g_local_unix_ms / 1000ULL;
  days = whole_seconds / 86400ULL;
  seconds_of_day = (uint32_t)(whole_seconds % 86400ULL);
  millisecond = (uint16_t)(g_local_unix_ms % 1000ULL);

  while (1) {
    uint32_t days_in_year = is_leap_year(year) ? 366U : 365U;
    if (days < days_in_year) {
      break;
    }
    days -= days_in_year;
    ++year;
  }

  for (month = 1U; month <= 12U; ++month) {
    uint32_t dim = days_in_month[month - 1U];
    if (month == 2U && is_leap_year(year)) {
      dim = 29U;
    }
    if (days < dim) {
      day = (uint8_t)(days + 1U);
      break;
    }
    days -= dim;
  }

  hour = (uint8_t)(seconds_of_day / 3600U);
  minute = (uint8_t)((seconds_of_day % 3600U) / 60U);
  second = (uint8_t)(seconds_of_day % 60U);

  return seds_router_set_local_network_datetime_millis(router, year, month, day, hour, minute,
                                                       second, millisecond);
}

static SedsResult refresh_local_unix_from_router(void) {
  SedsDateTime dt;
  if (!g_router.r) {
    return SEDS_ERR;
  }
  if (seds_router_get_network_time(g_router.r, &dt) != SEDS_OK) {
    return SEDS_ERR;
  }
  g_local_unix_ms = unix_ms_from_utc(dt.year, dt.month, dt.day, dt.hour, dt.minute,
                                     dt.second, dt.millisecond);
  g_local_unix_valid = 1U;
  return SEDS_OK;
}

SedsResult init_telemetry_router(void) {
  if (g_router.created && g_router.r) {
    return SEDS_OK;
  }

  const SedsLocalEndpointDesc locals[] = {
      {.endpoint = SEDS_EP_RADIO, .packet_handler = on_radio_packet, .user = NULL},
  };

  SedsRouter *r = seds_router_new(SEDS_RM_Sink, host_now_ms, NULL, locals,
                                  sizeof(locals) / sizeof(locals[0]));
  if (!r) {
    printf("Error: failed to create router\n");
    return SEDS_ERR;
  }

  if (seds_router_add_side_serialized(r, "TX", 2, tx_send, NULL, true) < 0) {
    printf("Error: failed to add router side\n");
    seds_router_free(r);
    return SEDS_ERR;
  }

  if (seds_router_configure_timesync(r, true, 1U, 10U, 5000U, 1000U, 1000U) != SEDS_OK) {
    printf("Error: failed to configure time sync\n");
    seds_router_free(r);
    return SEDS_ERR;
  }

  if (g_local_unix_valid) {
    (void)apply_local_unix_to_router(r);
  }

  g_router.r = r;
  g_router.created = 1U;
  g_router.start_time = host_now_ms(NULL);
  return SEDS_OK;
}

SedsResult log_telemetry_synchronous(SedsDataType data_type, const void *data,
                                     size_t element_count, size_t element_size) {
  if (!data || element_count == 0U || element_size == 0U) {
    return SEDS_BAD_ARG;
  }
  if (!g_router.r && init_telemetry_router() != SEDS_OK) {
    return SEDS_ERR;
  }
  return seds_router_log(g_router.r, data_type, data, element_count * element_size);
}

SedsResult log_telemetry_asynchronous(SedsDataType data_type, const void *data,
                                      size_t element_count, size_t element_size) {
  if (!data || element_count == 0U || element_size == 0U) {
    return SEDS_BAD_ARG;
  }
  if (!g_router.r && init_telemetry_router() != SEDS_OK) {
    return SEDS_ERR;
  }
  return seds_router_log_queue(g_router.r, data_type, data, element_count * element_size);
}

SedsResult log_telemetry_string_asynchronous(SedsDataType data_type, const char *str) {
  if (!str) {
    return SEDS_BAD_ARG;
  }
  if (!g_router.r && init_telemetry_router() != SEDS_OK) {
    return SEDS_ERR;
  }
  return seds_router_log_string_queue(g_router.r, data_type, str);
}

SedsResult dispatch_tx_queue(void) {
  if (!g_router.r && init_telemetry_router() != SEDS_OK) {
    return SEDS_ERR;
  }
  return seds_router_process_tx_queue(g_router.r);
}

SedsResult process_rx_queue(void) {
  if (!g_router.r && init_telemetry_router() != SEDS_OK) {
    return SEDS_ERR;
  }
  return seds_router_process_rx_queue(g_router.r);
}

SedsResult dispatch_tx_queue_timeout(uint32_t timeout_ms) {
  if (!g_router.r && init_telemetry_router() != SEDS_OK) {
    return SEDS_ERR;
  }
  return seds_router_process_tx_queue_with_timeout(g_router.r, timeout_ms);
}

SedsResult process_rx_queue_timeout(uint32_t timeout_ms) {
  if (!g_router.r && init_telemetry_router() != SEDS_OK) {
    return SEDS_ERR;
  }
  return seds_router_process_rx_queue_with_timeout(g_router.r, timeout_ms);
}

SedsResult process_all_queues_timeout(uint32_t timeout_ms) {
  if (!g_router.r && init_telemetry_router() != SEDS_OK) {
    return SEDS_ERR;
  }
  return seds_router_process_all_queues_with_timeout(g_router.r, timeout_ms);
}

SedsResult telemetry_periodic(uint32_t timeout_ms) {
  if (!g_router.r && init_telemetry_router() != SEDS_OK) {
    return SEDS_ERR;
  }
  SedsResult result = seds_router_periodic(g_router.r, timeout_ms);
  if (result == SEDS_OK) {
    (void)refresh_local_unix_from_router();
  }
  return result;
}

SedsResult telemetry_poll_timesync(void) {
  if (!g_router.r && init_telemetry_router() != SEDS_OK) {
    return SEDS_ERR;
  }
  SedsResult result = seds_router_poll_timesync(g_router.r, NULL);
  if (result == SEDS_OK) {
    (void)refresh_local_unix_from_router();
  }
  return result;
}

SedsResult telemetry_announce_discovery(void) {
  if (!g_router.r && init_telemetry_router() != SEDS_OK) {
    return SEDS_ERR;
  }
  return seds_router_announce_discovery(g_router.r);
}

SedsResult telemetry_poll_discovery(void) {
  if (!g_router.r && init_telemetry_router() != SEDS_OK) {
    return SEDS_ERR;
  }
  return seds_router_poll_discovery(g_router.r, NULL);
}

uint64_t telemetry_now_ms(void) { return host_now_ms(NULL); }

uint64_t telemetry_unix_ms(void) { return g_local_unix_valid ? g_local_unix_ms : 0ULL; }
uint64_t telemetry_unix_s(void) { return telemetry_unix_ms() / 1000ULL; }
uint8_t telemetry_unix_is_valid(void) { return g_local_unix_valid; }

void telemetry_set_unix_time_ms(uint64_t unix_ms) {
  g_local_unix_valid = unix_ms != 0ULL ? 1U : 0U;
  g_local_unix_ms = unix_ms;
  if (g_router.r) {
    (void)apply_local_unix_to_router(g_router.r);
  }
}

static SedsResult log_error_impl(uint8_t queue, const char *fmt, va_list args) {
  int written;
  char buf[512];

  if (!fmt) {
    return SEDS_BAD_ARG;
  }
  if (!g_router.r && init_telemetry_router() != SEDS_OK) {
    return SEDS_ERR;
  }

  written = vsnprintf(buf, sizeof(buf), fmt, args);
  if (written < 0) {
    return seds_router_log_string_ex(g_router.r, SEDS_DT_TELEMETRY_ERROR, "", 0U, NULL, queue);
  }

  return seds_router_log_string_ex(g_router.r, SEDS_DT_TELEMETRY_ERROR, buf,
                                   (size_t)((written < (int)sizeof(buf)) ? written : (int)sizeof(buf) - 1),
                                   NULL, queue);
}

SedsResult log_error_asynchronous(const char *fmt, ...) {
  va_list args;
  SedsResult result;
  va_start(args, fmt);
  result = log_error_impl(1U, fmt, args);
  va_end(args);
  return result;
}

SedsResult log_error_synchronous(const char *fmt, ...) {
  va_list args;
  SedsResult result;
  va_start(args, fmt);
  result = log_error_impl(0U, fmt, args);
  va_end(args);
  return result;
}

SedsResult log_error_asyncronous(const char *fmt, ...) {
  va_list args;
  SedsResult result;
  va_start(args, fmt);
  result = log_error_impl(1U, fmt, args);
  va_end(args);
  return result;
}

SedsResult log_error_syncronous(const char *fmt, ...) {
  va_list args;
  SedsResult result;
  va_start(args, fmt);
  result = log_error_impl(0U, fmt, args);
  va_end(args);
  return result;
}

SedsResult print_telemetry_error(const int32_t error_code) {
  const int32_t need = seds_error_to_string_len(error_code);
  if (need <= 0) {
    return (SedsResult)need;
  }

  char buf[(size_t)need];
  SedsResult res = seds_error_to_string(error_code, buf, sizeof(buf));
  if (res == SEDS_OK) {
    printf("Error: %s\n", buf);
  }
  return res;
}
