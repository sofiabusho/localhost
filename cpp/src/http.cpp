#include "http.hpp"

#include <algorithm>
#include <cctype>
#include <sstream>
#include <stdexcept>

namespace {

std::string to_lower(std::string s) {
  for (char& c : s) c = static_cast<char>(std::tolower(static_cast<unsigned char>(c)));
  return s;
}

size_t find_bytes(const std::vector<uint8_t>& hay, const char* needle, size_t from = 0) {
  size_t nlen = std::char_traits<char>::length(needle);
  if (nlen == 0 || hay.size() < from + nlen) return std::string::npos;
  for (size_t i = from; i + nlen <= hay.size(); ++i) {
    bool ok = true;
    for (size_t j = 0; j < nlen; ++j) {
      if (hay[i + j] != static_cast<uint8_t>(needle[j])) {
        ok = false;
        break;
      }
    }
    if (ok) return i;
  }
  return std::string::npos;
}

bool decode_chunked(const std::vector<uint8_t>& src, size_t& used, uint64_t max_body,
                    std::vector<uint8_t>& out) {
  size_t i = 0;
  out.clear();
  while (i < src.size()) {
    size_t line_end = find_bytes(src, "\r\n", i);
    if (line_end == std::string::npos) return false;
    std::string hex;
    for (size_t k = i; k < line_end; ++k) hex.push_back(static_cast<char>(src[k]));
    size_t semi = hex.find(';');
    if (semi != std::string::npos) hex = hex.substr(0, semi);
    size_t chunk = 0;
    try {
      chunk = std::stoul(hex, nullptr, 16);
    } catch (...) {
      throw std::runtime_error("bad chunk size");
    }
    i = line_end + 2;
    if (chunk == 0) {
      // trailers until blank line
      while (true) {
        size_t te = find_bytes(src, "\r\n", i);
        if (te == std::string::npos) return false;
        if (te == i) {
          used = te + 2;
          return true;
        }
        i = te + 2;
      }
    }
    if (out.size() + chunk > max_body) throw std::runtime_error("payload too large");
    if (i + chunk + 2 > src.size()) return false;
    out.insert(out.end(), src.begin() + static_cast<long>(i),
               src.begin() + static_cast<long>(i + chunk));
    i += chunk;
    if (i + 2 > src.size() || src[i] != '\r' || src[i + 1] != '\n')
      throw std::runtime_error("bad chunk framing");
    i += 2;
  }
  return false;
}

}  // namespace

std::string reason_phrase(int code) {
  switch (code) {
    case 200: return "OK";
    case 201: return "Created";
    case 204: return "No Content";
    case 301: return "Moved Permanently";
    case 302: return "Found";
    case 400: return "Bad Request";
    case 403: return "Forbidden";
    case 404: return "Not Found";
    case 405: return "Method Not Allowed";
    case 413: return "Payload Too Large";
    case 500: return "Internal Server Error";
    case 504: return "Gateway Timeout";
    default: return "Error";
  }
}

std::string header_get(const Inbound& req, const std::string& name) {
  auto it = req.headers.find(to_lower(name));
  if (it == req.headers.end()) return {};
  return it->second;
}

std::string path_only(const std::string& target) {
  auto q = target.find('?');
  auto h = target.find('#');
  size_t end = target.size();
  if (q != std::string::npos) end = std::min(end, q);
  if (h != std::string::npos) end = std::min(end, h);
  std::string p = target.substr(0, end);
  return p.empty() ? "/" : p;
}

Outbound make_text(int status, const std::string& body) {
  Outbound o;
  o.status = status;
  o.headers.emplace_back("Content-Type", "text/plain; charset=utf-8");
  o.body.assign(body.begin(), body.end());
  return o;
}

Outbound make_html(int status, const std::string& body) {
  Outbound o;
  o.status = status;
  o.headers.emplace_back("Content-Type", "text/html; charset=utf-8");
  o.body.assign(body.begin(), body.end());
  return o;
}

Outbound stock_error(int status) {
  std::ostringstream body;
  body << "<!DOCTYPE html><html><head><title>" << status << " " << reason_phrase(status)
       << "</title></head><body><h1>" << status << " " << reason_phrase(status)
       << "</h1><p>localhost_cpp</p></body></html>\n";
  return make_html(status, body.str());
}

std::vector<uint8_t> serialize_response(const Outbound& resp) {
  std::ostringstream head;
  head << "HTTP/1.1 " << resp.status << " " << reason_phrase(resp.status) << "\r\n";
  bool has_len = false;
  bool has_conn = false;
  for (const auto& [k, v] : resp.headers) {
    if (to_lower(k) == "content-length") has_len = true;
    if (to_lower(k) == "connection") has_conn = true;
    head << k << ": " << v << "\r\n";
  }
  if (!has_len) head << "Content-Length: " << resp.body.size() << "\r\n";
  if (!has_conn) head << "Connection: close\r\n";
  head << "\r\n";
  std::string hs = head.str();
  std::vector<uint8_t> out(hs.begin(), hs.end());
  out.insert(out.end(), resp.body.begin(), resp.body.end());
  return out;
}

std::optional<std::pair<Inbound, size_t>> try_parse_request(const std::vector<uint8_t>& buf,
                                                           uint64_t max_body) {
  size_t head_end = find_bytes(buf, "\r\n\r\n");
  if (head_end == std::string::npos) {
    if (buf.size() > 64 * 1024) throw std::runtime_error("bad request");
    return std::nullopt;
  }
  std::string head(reinterpret_cast<const char*>(buf.data()), head_end);
  size_t line_end = head.find("\r\n");
  if (line_end == std::string::npos) throw std::runtime_error("bad request");
  std::string reqline = head.substr(0, line_end);
  std::istringstream rl(reqline);
  Inbound msg;
  if (!(rl >> msg.method >> msg.target >> msg.version)) throw std::runtime_error("bad request");
  if (msg.version.rfind("HTTP/", 0) != 0) throw std::runtime_error("bad request");

  size_t pos = line_end + 2;
  while (pos < head.size()) {
    size_t nl = head.find("\r\n", pos);
    if (nl == std::string::npos) break;
    std::string line = head.substr(pos, nl - pos);
    pos = nl + 2;
    if (line.empty()) break;
    auto colon = line.find(':');
    if (colon == std::string::npos) throw std::runtime_error("bad request");
    std::string name = to_lower(line.substr(0, colon));
    std::string value = line.substr(colon + 1);
    while (!value.empty() && value[0] == ' ') value.erase(value.begin());
    msg.headers[name] = value;
  }

  size_t body_start = head_end + 4;
  auto te = msg.headers.find("transfer-encoding");
  auto cl = msg.headers.find("content-length");
  if (te != msg.headers.end() && to_lower(te->second) == "chunked") {
    if (cl != msg.headers.end()) throw std::runtime_error("bad request");
    std::vector<uint8_t> slice(buf.begin() + static_cast<long>(body_start), buf.end());
    size_t used = 0;
    if (!decode_chunked(slice, used, max_body, msg.body)) return std::nullopt;
    return std::make_pair(msg, body_start + used);
  }
  if (cl != msg.headers.end()) {
    size_t len = 0;
    try {
      len = std::stoul(cl->second);
    } catch (...) {
      throw std::runtime_error("bad request");
    }
    if (static_cast<uint64_t>(len) > max_body) throw std::runtime_error("payload too large");
    if (buf.size() < body_start + len) return std::nullopt;
    msg.body.assign(buf.begin() + static_cast<long>(body_start),
                    buf.begin() + static_cast<long>(body_start + len));
    return std::make_pair(msg, body_start + len);
  }
  return std::make_pair(msg, body_start);
}
