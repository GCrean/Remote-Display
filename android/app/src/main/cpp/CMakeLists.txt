cmake_minimum_required(VERSION 3.22.1)
project(rdcore LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

add_library(rdcore SHARED
    server.cpp
    encoder.cpp
    jni.cpp)

target_compile_options(rdcore PRIVATE -Wall -Wextra -O2 -fvisibility=hidden -fno-exceptions)

# Android 15+ devices use 16 KB pages; required for targetSdk 35 apps shipping native code.
target_link_options(rdcore PRIVATE -Wl,-z,max-page-size=16384)

# mediandk: AMediaCodec/AMediaFormat. android: ANativeWindow/JNI helpers. log: logcat.
target_link_libraries(rdcore PRIVATE mediandk android log)
