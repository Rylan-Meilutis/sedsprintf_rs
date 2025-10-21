# SEDSPRINTF_RS

An implementation of the `sedsprintf` library in Rust.

## Overview

This library provides a safe and efficient way to handle telemetry data including serializing, routing, and converting
into strings for logging. The purpose of this library is to provide an easy and consistent way to create and transport
any data to any destination without needing to implement the core routing on every platform, the library handles the
creation and routing of the data while allowing for easy extension to support new data types and destinations. The user
of the library is responsible for implementing 2 core functions and one function per local data endpoint.
The core functions are as follows:

- A function to send raw bytes to the all other nodes (e.g. UART, SPI, I2C, CAN, etc.)
- A function to receive raw bytes from all other nodes and passes the packet to the router
  (e.g. UART, SPI, I2C, CAN, etc.)
- A function to handle local data endpoints (e.g. logging to console, writing to file, sending over radio, etc.)
  (Note: each local endpoint needs its own function)

The library also provides helpers to convert the telemetry data into strings for logging purposes. the library also
handles the serialization and deserialization of the telemetry data.

The library is platform-agnostic and can be used on any platform that supports Rust. The library is primarily designed
to be used in embedded systems and used by a C program, but can also be used in desktop applications and other rust
codebases.

## Dependencies

- Rust

  get it from https://rustup.rs/
- Cmake
- A C++ compiler
- A C compiler
- The thumbv7m-none-eabi target for Rust (if you want to build for ARM Cortex-M)

  Get it with:
  ```bash
  rustup target add thumbv7em-none-eabihf
  ```

## Usage

- When using this library as a submodule or subtree in a C or C++ project, make sure to add the following to your
- cmakelists.txt
  ```cmake
  add_subdirectory(${CMAKE_SOURCE_DIR}/sedsprintf_rs)
  target_link_libraries(${CMAKE_PROJECT_NAME} sedsprintf_rs)
  ```


- To add this repo as a subtree to allow for modifications, use the following command:
  ```bash
  git remote add sedsprintf-upstream https://github.com/Rylan-Meilutis/sedsprintf_rs.git
  git fetch sedsprintf-upstream
  git subtree add --prefix=sedsprintf_rs sedsprintf-upstream main

- To update the subtree, use the following command (Note all local changes must be committed before you can update):
  ```bash
  git subtree pull --prefix=sedsprintf_rs sedsprintf-upstream main -m "Merge sedsprintf_rs upstream main"
  ```
