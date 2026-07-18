#pragma once

#include "types.hpp"
#include <optional>
#include <string>
#include <vector>

std::optional<std::pair<Inbound, size_t>> try_parse_request(
    const std::vector<uint8_t>& buf, uint64_t max_body);

std::vector<uint8_t> serialize_response(const Outbound& resp);

Outbound make_text(int status, const std::string& body);
Outbound make_html(int status, const std::string& body);
Outbound stock_error(int status);

std::string reason_phrase(int code);
std::string header_get(const Inbound& req, const std::string& name);
std::string path_only(const std::string& target);
