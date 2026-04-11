cmake_minimum_required(VERSION 3.22)
project(c_system_test C)

set(CMAKE_C_STANDARD 11)

if (APPLE AND DEFINED ENV{MACOSX_DEPLOYMENT_TARGET} AND NOT "$ENV{MACOSX_DEPLOYMENT_TARGET}" STREQUAL "")
    # Keep CMake and Rust on the same deployment target when one is explicitly provided.
    set(CMAKE_OSX_DEPLOYMENT_TARGET "$ENV{MACOSX_DEPLOYMENT_TARGET}" CACHE STRING "" FORCE)
endif ()

# 1) Define the target first
add_executable(c_system_test
        src/main.c
        src/telemetry_sim.c
)

add_executable(c_system_timesync_test
        src/timesync_multinode_test.c
        src/telemetry_sim.c
)

add_executable(c_system_board_topology_timesync_test
        src/board_topology_timesync_test.c
        src/telemetry_sim.c
)

add_executable(c_system_multi_endpoint_topology_test
        src/multi_endpoint_topology_test.c
)

# 2) Local includes (your own headers)
target_include_directories(c_system_test PRIVATE
        "${CMAKE_CURRENT_SOURCE_DIR}/include"
)
target_include_directories(c_system_timesync_test PRIVATE
        "${CMAKE_CURRENT_SOURCE_DIR}/include"
)
target_include_directories(c_system_board_topology_timesync_test PRIVATE
        "${CMAKE_CURRENT_SOURCE_DIR}/include"
)
target_include_directories(c_system_multi_endpoint_topology_test PRIVATE
        "${CMAKE_CURRENT_SOURCE_DIR}/include"
)

# 3) Bring in the Rust crate (parent dir) with an explicit build dir
get_filename_component(SEDSPRINTF_RS_ROOT "${CMAKE_CURRENT_LIST_DIR}/.." REALPATH)
set(SEDSPRINTF_RS_DEVICE_IDENTIFIER "SYSTEM_TEST" CACHE STRING "" FORCE)
add_subdirectory(
        "${SEDSPRINTF_RS_ROOT}"
        "${CMAKE_CURRENT_BINARY_DIR}/sedsprintf_rs-build"
        EXCLUDE_FROM_ALL
)

# 4) Link + add the crate's public headers
target_link_libraries(c_system_test PRIVATE sedsprintf_rs)
target_include_directories(c_system_test PRIVATE "${SEDSPRINTF_RS_INCLUDE_DIR}")
target_link_libraries(c_system_timesync_test PRIVATE sedsprintf_rs)
target_include_directories(c_system_timesync_test PRIVATE "${SEDSPRINTF_RS_INCLUDE_DIR}")
target_link_libraries(c_system_board_topology_timesync_test PRIVATE sedsprintf_rs)
target_include_directories(c_system_board_topology_timesync_test PRIVATE "${SEDSPRINTF_RS_INCLUDE_DIR}")
target_link_libraries(c_system_multi_endpoint_topology_test PRIVATE sedsprintf_rs)
target_include_directories(c_system_multi_endpoint_topology_test PRIVATE "${SEDSPRINTF_RS_INCLUDE_DIR}")
