#pragma once

#include "types.hpp"
#include <string>

SiteBundle load_config(const std::string& path);
void validate_config(const SiteBundle& bundle);
