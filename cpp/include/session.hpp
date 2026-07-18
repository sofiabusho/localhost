#pragma once

#include <cstdint>
#include <string>

// Process-wide cookie sessions (single-threaded hub).
std::string session_touch(const std::string& cookie_header, uint64_t& hits_out);
std::string set_cookie_header(const std::string& sid);
void session_sweep();
