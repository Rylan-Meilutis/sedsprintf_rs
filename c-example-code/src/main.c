#include "telemetry.h"
#include "sedsprintf.h"
#include <unistd.h>

int main(void) {
  SedsResult result = init_telemetry_router();
  if (result != SEDS_OK) {
    return (int)print_telemetry_error(result);
  }

  const float gps[3] = {37.7749f, -122.4194f, 30.0f};
  result = log_telemetry_synchronous(SEDS_DT_GPS_DATA, gps, 3, sizeof(gps[0]));
  if (result != SEDS_OK) {
    print_telemetry_error(result);
  }

  usleep(1000);

  result = log_telemetry_asynchronous(SEDS_DT_GPS_DATA, gps, 3, sizeof(gps[0]));
  if (result != SEDS_OK) {
    print_telemetry_error(result);
  }

  result = log_telemetry_string_asynchronous(SEDS_DT_MESSAGE_DATA, "hello from the async queue");
  if (result != SEDS_OK) {
    print_telemetry_error(result);
  }

  result = telemetry_periodic(20);
  if (result != SEDS_OK) {
    print_telemetry_error(result);
  }

  return 0;
}
