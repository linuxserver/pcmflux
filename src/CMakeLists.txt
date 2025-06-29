cmake_minimum_required(VERSION 3.14)
set(CMAKE_BUILD_TYPE Release)
set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED TRUE)
set(CMAKE_POSITION_INDEPENDENT_CODE ON)
project(audio_capture_module_cpp)
include(FetchContent)

# --- Fetch Opus from source for portability ---
set(FC_COMMON_CMAKE_ARGS
    -DCMAKE_BUILD_TYPE=${CMAKE_BUILD_TYPE}
    -DCMAKE_POSITION_INDEPENDENT_CODE=ON
    -DCMAKE_C_COMPILER=${CMAKE_C_COMPILER}
    -DCMAKE_CXX_COMPILER=${CMAKE_CXX_COMPILER}
    -DCMAKE_C_FLAGS=${CMAKE_C_FLAGS}
    -DCMAKE_CXX_FLAGS=${CMAKE_CXX_FLAGS}
)

FetchContent_Declare(
    opus_dep
    GIT_REPOSITORY https://github.com/xiph/opus.git
    GIT_TAG v1.4 # Use a stable tag
    CMAKE_ARGS ${FC_COMMON_CMAKE_ARGS} -DBUILD_SHARED_LIBS=OFF -DOPUS_BUILD_TESTING=OFF -DOPUS_BUILD_PROGRAMS=OFF
    FETCHCONTENT_QUIET FALSE
)
FetchContent_MakeAvailable(opus_dep)

# --- Find system libraries ---
find_package(PkgConfig REQUIRED)
pkg_check_modules(PulseSimple REQUIRED libpulse-simple)
find_package(Python3 COMPONENTS Interpreter Development REQUIRED)
find_package(Threads REQUIRED)

# --- Build the shared module ---
add_library(audio_capture_module SHARED
    audio_capture_module.cpp
)

target_include_directories(audio_capture_module PRIVATE
    ${Python3_INCLUDE_DIRS}
    ${PulseSimple_INCLUDE_DIRS}
    "${opus_dep_SOURCE_DIR}/include"
)

target_link_libraries(audio_capture_module PRIVATE
    Python3::Python
    Threads::Threads
    ${PulseSimple_LIBRARIES}
    opus
)

set_target_properties(audio_capture_module PROPERTIES
    PREFIX ""
    SUFFIX ".so"
    OUTPUT_NAME "audio_capture_module"
)

install(TARGETS audio_capture_module
    LIBRARY DESTINATION pcmflux
    COMPONENT audio_capture_runtime
)
