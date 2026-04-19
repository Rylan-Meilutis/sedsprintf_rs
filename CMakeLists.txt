cmake_minimum_required(VERSION 3.22)
project(sedsprintf_rs_cmake NONE)

# --- Find Python interpreter ---
find_package(Python3 COMPONENTS Interpreter REQUIRED)
set(PYTHON_EXECUTABLE "${Python3_EXECUTABLE}")

set(SEDSPRINTF_RS_DIR_DEFAULT "${CMAKE_CURRENT_LIST_DIR}/")
set(SEDSPRINTF_RS_DIR "${SEDSPRINTF_RS_DIR_DEFAULT}" CACHE PATH "Path to sedsprintf_rs crate root")
get_filename_component(RUST_DIR "${SEDSPRINTF_RS_DIR}" REALPATH)

# ============================================================
# Config overrides forwarded into the Rust build (via build.py)
# ============================================================

# Device identifier override for Rust (passed to build.py)
set(SEDSPRINTF_RS_DEVICE_IDENTIFIER "" CACHE STRING
    "Override DEVICE_IDENTIFIER passed to build.py (empty = use default)")

# New: proc-macro env var (define_stack_payload!(env = \"MAX_STACK_PAYLOAD\", default = ...))
set(SEDSPRINTF_RS_MAX_STACK_PAYLOAD "" CACHE STRING
    "Override MAX_STACK_PAYLOAD used by define_stack_payload! (empty = use macro default)")

set(SEDSPRINTF_RS_MAX_QUEUE_BUDGET "" CACHE STRING
    "Override MAX_QUEUE_BUDGET shared queue budget in bytes (empty = use Rust default)")

set(SEDSPRINTF_RS_MAX_QUEUE_SIZE "" CACHE STRING
    "Legacy alias for SEDSPRINTF_RS_MAX_QUEUE_BUDGET (empty = ignored)")

set(SEDSPRINTF_RS_MAX_RECENT_RX_IDS "" CACHE STRING
    "Override MAX_RECENT_RX_IDS preallocated recent packet ID entries (empty = use Rust default)")

# Schema JSON path override (forwarded as SEDSPRINTF_RS_SCHEMA_PATH)
set(SEDSPRINTF_RS_SCHEMA_PATH "" CACHE STRING
    "Override telemetry_config.json path (empty = use default)")

# For each env var you want to set, define a CMake var named:
#   SEDSPRINTF_RS_ENV_<ENVKEY>
# Example:
#   set(SEDSPRINTF_RS_ENV_QUEUE_GROW_STEP "2.0" CACHE STRING "" FORCE)
# Prefer the dedicated cache variables above for MAX_QUEUE_BUDGET and MAX_RECENT_RX_IDS.
#
# These will be forwarded as build.py args: env:ENVKEY=VALUE

# ============================================================
# Detect embedded build
# ============================================================
option(SEDSPRINTF_EMBEDDED_BUILD "Build sedsprintf_rs for embedded" OFF)
option(SEDSPRINTF_RS_PREFER_DYNAMIC "Prefer dynamic linking for host builds when supported" ON)
option(SEDSPRINTF_RS_FORCE_RELEASE "Build sedsprintf_rs in release profile even when the parent CMake build is Debug" OFF)

# Auto-detect embedded if user hasn't forced it
if (NOT SEDSPRINTF_EMBEDDED_BUILD)
    if (CMAKE_C_COMPILER MATCHES "arm-none-eabi"
            OR CMAKE_TOOLCHAIN_FILE MATCHES "stm32"
            OR CMAKE_TOOLCHAIN_FILE MATCHES "gcc-arm-none-eabi"
            OR DEFINED ENV{STM32CUBEIDE_DIR}
    )
        set(SEDSPRINTF_EMBEDDED_BUILD ON CACHE BOOL "Build sedsprintf_rs for embedded" FORCE)
    endif ()
endif ()

# ============================================================
# Rust target triple selection
# ============================================================
if (NOT DEFINED SEDSPRINTF_RS_TARGET)
    if (SEDSPRINTF_EMBEDDED_BUILD)
        set(SEDSPRINTF_RS_TARGET "thumbv7em-none-eabihf" CACHE STRING "Rust target triple for sedsprintf_rs (empty = host default)")
    else ()
        set(SEDSPRINTF_RS_TARGET "" CACHE STRING "Rust target triple for sedsprintf_rs (empty = host default)")
    endif ()
endif ()

set(RUST_TRIPLE "${SEDSPRINTF_RS_TARGET}")

# ============================================================
# Compute Rust output paths
# ============================================================
set(RELEASE_PROFILE_DIR "release")
if (SEDSPRINTF_EMBEDDED_BUILD)
    set(RELEASE_PROFILE_DIR "release-embedded")
endif ()

if (SEDSPRINTF_RS_FORCE_RELEASE)
    set(SEDSPRINTF_RS_ACTIVE_PROFILE "${RELEASE_PROFILE_DIR}")
else ()
    set(SEDSPRINTF_RS_ACTIVE_PROFILE "$<IF:$<CONFIG:Debug>,debug,${RELEASE_PROFILE_DIR}>")
endif ()

if (RUST_TRIPLE STREQUAL "")
    set(RS_STATIC_LIB_DBG "${RUST_DIR}/target/debug/libsedsprintf_rs.a")
    set(RS_STATIC_LIB_REL "${RUST_DIR}/target/release/libsedsprintf_rs.a")
else ()
    set(RS_STATIC_LIB_DBG "${RUST_DIR}/target/${RUST_TRIPLE}/debug/libsedsprintf_rs.a")
    set(RS_STATIC_LIB_REL "${RUST_DIR}/target/${RUST_TRIPLE}/${RELEASE_PROFILE_DIR}/libsedsprintf_rs.a")
endif ()

set(RS_LIB_DBG "${RS_STATIC_LIB_DBG}")
set(RS_LIB_REL "${RS_STATIC_LIB_REL}")
if (SEDSPRINTF_RS_FORCE_RELEASE)
    set(RS_LIB_DBG "${RS_LIB_REL}")
endif ()
set(RS_IMPORTED_KIND STATIC)
set(SEDSPRINTF_RS_LINK_DYNAMIC OFF)
set(RS_SHARED_LIB_DBG "")
set(RS_SHARED_LIB_REL "")

set(RS_INCLUDE "${RUST_DIR}/C-Headers")
set(RS_HEADER "${RS_INCLUDE}/sedsprintf.h")

# ============================================================
# Build.py args (for logging only)
# ============================================================
set(RS_ARGS_STR "")
if (SEDSPRINTF_EMBEDDED_BUILD)
    set(RS_ARGS_STR "${RS_ARGS_STR} embedded")
endif ()
if (NOT RUST_TRIPLE STREQUAL "")
    set(RS_ARGS_STR "${RS_ARGS_STR} target=${RUST_TRIPLE}")
endif ()
if (NOT SEDSPRINTF_RS_DEVICE_IDENTIFIER STREQUAL "")
    set(RS_ARGS_STR "${RS_ARGS_STR} device_id=${SEDSPRINTF_RS_DEVICE_IDENTIFIER}")
endif ()
if (NOT SEDSPRINTF_RS_MAX_STACK_PAYLOAD STREQUAL "")
    set(RS_ARGS_STR "${RS_ARGS_STR} max_stack_payload=${SEDSPRINTF_RS_MAX_STACK_PAYLOAD}")
endif ()
set(_SEDSPRINTF_EFFECTIVE_MAX_QUEUE_BUDGET "${SEDSPRINTF_RS_MAX_QUEUE_BUDGET}")
if (_SEDSPRINTF_EFFECTIVE_MAX_QUEUE_BUDGET STREQUAL "" AND NOT SEDSPRINTF_RS_MAX_QUEUE_SIZE STREQUAL "")
    set(_SEDSPRINTF_EFFECTIVE_MAX_QUEUE_BUDGET "${SEDSPRINTF_RS_MAX_QUEUE_SIZE}")
endif ()
if (NOT _SEDSPRINTF_EFFECTIVE_MAX_QUEUE_BUDGET STREQUAL "")
    set(RS_ARGS_STR "${RS_ARGS_STR} max_queue_budget=${_SEDSPRINTF_EFFECTIVE_MAX_QUEUE_BUDGET}")
endif ()
if (NOT SEDSPRINTF_RS_MAX_RECENT_RX_IDS STREQUAL "")
    set(RS_ARGS_STR "${RS_ARGS_STR} max_recent_rx_ids=${SEDSPRINTF_RS_MAX_RECENT_RX_IDS}")
endif ()
if (NOT SEDSPRINTF_RS_SCHEMA_PATH STREQUAL "")
    set(RS_ARGS_STR "${RS_ARGS_STR} schema_path=${SEDSPRINTF_RS_SCHEMA_PATH}")
endif ()

if (SEDSPRINTF_RS_FORCE_RELEASE)
    set(RS_ARGS_STR "${RS_ARGS_STR} release")
else ()
    # Show release in log string
    set(RS_ARG_REL " $<$<CONFIG:Release>:release>")
    set(RS_ARGS_STR "${RS_ARGS_STR} ${RS_ARG_REL}")
endif ()

# ============================================================
# Configure-time args passed to build.py (list form)
# ============================================================
set(RS_ARGS_BOOTSTRAP "")

if (SEDSPRINTF_EMBEDDED_BUILD)
    list(APPEND RS_ARGS_BOOTSTRAP "embedded")
endif ()

if (NOT RUST_TRIPLE STREQUAL "")
    list(APPEND RS_ARGS_BOOTSTRAP "target=${RUST_TRIPLE}")
endif ()

# NOTE: When using multi-config generators (VS/Xcode), CMAKE_BUILD_TYPE is empty.
# We rely on the build step to rebuild as needed anyway; default to Debug.
if (SEDSPRINTF_RS_FORCE_RELEASE OR CMAKE_BUILD_TYPE STREQUAL "Release")
    list(APPEND RS_ARGS_BOOTSTRAP "release")
endif ()

if (NOT SEDSPRINTF_RS_DEVICE_IDENTIFIER STREQUAL "")
    list(APPEND RS_ARGS_BOOTSTRAP "device_id=${SEDSPRINTF_RS_DEVICE_IDENTIFIER}")
endif ()

# New: forward proc-macro env var
if (NOT SEDSPRINTF_RS_MAX_STACK_PAYLOAD STREQUAL "")
    list(APPEND RS_ARGS_BOOTSTRAP "max_stack_payload=${SEDSPRINTF_RS_MAX_STACK_PAYLOAD}")
endif ()
if (NOT _SEDSPRINTF_EFFECTIVE_MAX_QUEUE_BUDGET STREQUAL "")
    list(APPEND RS_ARGS_BOOTSTRAP "max_queue_budget=${_SEDSPRINTF_EFFECTIVE_MAX_QUEUE_BUDGET}")
endif ()
if (NOT SEDSPRINTF_RS_MAX_RECENT_RX_IDS STREQUAL "")
    list(APPEND RS_ARGS_BOOTSTRAP "max_recent_rx_ids=${SEDSPRINTF_RS_MAX_RECENT_RX_IDS}")
endif ()
if (NOT SEDSPRINTF_RS_SCHEMA_PATH STREQUAL "")
    list(APPEND RS_ARGS_BOOTSTRAP "schema_path=${SEDSPRINTF_RS_SCHEMA_PATH}")
endif ()

# New: forward arbitrary option_env config vars: SEDSPRINTF_RS_ENV_<KEY> => env:KEY=VALUE
get_cmake_property(_all_cache_vars CACHE_VARIABLES)
foreach (_v ${_all_cache_vars})
    if (_v MATCHES "^SEDSPRINTF_RS_ENV_(.+)$")
        string(REGEX REPLACE "^SEDSPRINTF_RS_ENV_" "" _env_key "${_v}")
        # Value comes from the cache variable
        set(_env_val "${${_v}}")
        if (NOT _env_val STREQUAL "")
            list(APPEND RS_ARGS_BOOTSTRAP "env:${_env_key}=${_env_val}")
            # Also append to logging string
            set(RS_ARGS_STR "${RS_ARGS_STR} env:${_env_key}=${_env_val}")
        endif ()
    endif ()
endforeach ()

# Ensure include dir exists
file(MAKE_DIRECTORY "${RS_INCLUDE}")

# Keep Rust staticlib object deployment target aligned with C/C++ link target on macOS.
set(_RS_ENV_CMD ${CMAKE_COMMAND} -E env)
if (APPLE AND CMAKE_OSX_DEPLOYMENT_TARGET)
    list(APPEND _RS_ENV_CMD "MACOSX_DEPLOYMENT_TARGET=${CMAKE_OSX_DEPLOYMENT_TARGET}")
endif ()

# ============================================================
# Bootstrap build if header missing
# ============================================================
if (NOT EXISTS "${RS_HEADER}")
    message(STATUS "sedsprintf_rs: header not found (${RS_HEADER}); building once…")
    execute_process(
        COMMAND ${_RS_ENV_CMD} "${PYTHON_EXECUTABLE}" build.py ${RS_ARGS_BOOTSTRAP}
        WORKING_DIRECTORY "${RUST_DIR}"
        RESULT_VARIABLE _hdr_build_res
    )
    if (NOT _hdr_build_res EQUAL 0)
        message(WARNING "Header bootstrap failed (exit ${_hdr_build_res}); will rely on full build step.")
    endif ()
endif ()

# ============================================================
# Track Rust source inputs
# ============================================================
file(GLOB_RECURSE RS_SRC CONFIGURE_DEPENDS
    "${RUST_DIR}/src/*.rs"
    "${RUST_DIR}/build.rs"
    "${RUST_DIR}/Cargo.toml"
    "${RUST_DIR}/Cargo.lock"
)

if (SEDSPRINTF_RS_FORCE_RELEASE)
    set(RS_BUILD_OUTPUTS "${RS_LIB_REL}" "${RS_HEADER}")
else ()
    set(RS_BUILD_OUTPUTS "${RS_LIB_DBG}" "${RS_LIB_REL}" "${RS_HEADER}")
endif ()

# ============================================================
# Main build rule
# ============================================================
add_custom_command(
    OUTPUT ${RS_BUILD_OUTPUTS}
    COMMAND ${_RS_ENV_CMD} "${PYTHON_EXECUTABLE}" build.py ${RS_ARGS_BOOTSTRAP}
    WORKING_DIRECTORY "${RUST_DIR}"
    DEPENDS ${RS_SRC}
    COMMENT "Building sedsprintf_rs (${SEDSPRINTF_RS_ACTIVE_PROFILE}) ${RS_ARGS_STR}"
    USES_TERMINAL
    VERBATIM
)

add_custom_target(sedsprintf_rs_build
    DEPENDS ${RS_BUILD_OUTPUTS}
)

# ============================================================
# Import library for C/C++
# ============================================================
add_library(sedsprintf_rs ${RS_IMPORTED_KIND} IMPORTED GLOBAL)
set_target_properties(sedsprintf_rs PROPERTIES
    IMPORTED_LOCATION_DEBUG "${RS_LIB_DBG}"
    IMPORTED_LOCATION_RELEASE "${RS_LIB_REL}"
    INTERFACE_INCLUDE_DIRECTORIES "${RS_INCLUDE}"
)

add_dependencies(sedsprintf_rs sedsprintf_rs_build)

# Security flags for embedded
include(CheckLinkerFlag)
include(CheckCCompilerFlag)
check_linker_flag(C "-Wl,-z,noexecstack" HAVE_LD_NOEXECSTACK)
check_c_compiler_flag("-Wa,--noexecstack" HAVE_AS_NOEXECSTACK)

if (SEDSPRINTF_EMBEDDED_BUILD)
    target_compile_options(sedsprintf_rs INTERFACE -Wa,--noexecstack)
    target_link_options(sedsprintf_rs INTERFACE -Wl,-z,noexecstack)
endif ()

# Optimize
add_compile_options(-ffunction-sections -fdata-sections -Os)
add_link_options(-Wl,--gc-sections -Wl,--icf=all)

add_library(sedsprintf_rs::sedsprintf_rs ALIAS sedsprintf_rs)

set(SEDSPRINTF_RS_LIB_DEBUG "${RS_LIB_DBG}" PARENT_SCOPE)
set(SEDSPRINTF_RS_LIB_RELEASE "${RS_LIB_REL}" PARENT_SCOPE)
set(SEDSPRINTF_RS_STATIC_LIB_DEBUG "${RS_STATIC_LIB_DBG}" PARENT_SCOPE)
set(SEDSPRINTF_RS_STATIC_LIB_RELEASE "${RS_STATIC_LIB_REL}" PARENT_SCOPE)
set(SEDSPRINTF_RS_SHARED_LIB_DEBUG "${RS_SHARED_LIB_DBG}" PARENT_SCOPE)
set(SEDSPRINTF_RS_SHARED_LIB_RELEASE "${RS_SHARED_LIB_REL}" PARENT_SCOPE)
set(SEDSPRINTF_RS_INCLUDE_DIR "${RS_INCLUDE}" PARENT_SCOPE)

message(STATUS "sedsprintf_rs include dir: ${RS_INCLUDE}")
message(STATUS "sedsprintf_rs debug lib:  ${RS_LIB_DBG}")
message(STATUS "sedsprintf_rs release lib:${RS_LIB_REL}")
if (SEDSPRINTF_RS_LINK_DYNAMIC)
    set(_SEDSPRINTF_RS_LINK_MODE "dynamic")
else ()
    set(_SEDSPRINTF_RS_LINK_MODE "static")
endif ()
message(STATUS "sedsprintf_rs link mode: ${_SEDSPRINTF_RS_LINK_MODE}")
message(STATUS "sedsprintf_rs build args: ${RS_ARGS_STR}")
