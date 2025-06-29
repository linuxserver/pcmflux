cmake_minimum_required(VERSION 3.14)
set(CMAKE_BUILD_TYPE Release)
set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED TRUE)
set(CMAKE_POSITION_INDEPENDENT_CODE ON)
project(audio_capture_module_cpp)

# --- Find all required system libraries using pkg-config ---
find_package(PkgConfig REQUIRED)
pkg_check_modules(PulseSimple REQUIRED libpulse-simple)
pkg_check_modules(OPUS REQUIRED opus)

# --- Find Python and Threads ---
find_package(Python3 COMPONENTS Interpreter Development REQUIRED)
find_package(Threads REQUIRED)

# --- Build the shared module ---
add_library(audio_capture_module SHARED
    audio_capture_module.cpp
)

# --- Link against the found libraries ---
target_include_directories(audio_capture_module PRIVATE
    ${Python3_INCLUDE_DIRS}
    ${PulseSimple_INCLUDE_DIRS}
    ${OPUS_INCLUDE_DIRS} # <-- USE THE FOUND OPUS PATH
)

target_link_libraries(audio_capture_module PRIVATE
    Python3::Python
    Threads::Threads
    ${PulseSimple_LIBRARIES}
    ${OPUS_LIBRARIES} # <-- USE THE FOUND OPUS LIBRARY
)

# --- Standard properties and installation ---
set_target_properties(audio_capture_module PROPERTIES
    PREFIX ""
    SUFFIX ".so"
    OUTPUT_NAME "audio_capture_module"
)

install(TARGETS audio_capture_module
    LIBRARY DESTINATION pcmflux
    COMPONENT audio_capture_runtime
)
