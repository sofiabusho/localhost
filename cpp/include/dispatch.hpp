#pragma once

#include "types.hpp"
#include <string>

Outbound dispatch(const SiteBundle& bundle, const std::string& listen_addr,
                  const Inbound& req);
