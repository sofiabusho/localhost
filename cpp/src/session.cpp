#include "session.hpp"

#include <cctype>
#include <chrono>
#include <map>
#include <sstream>

namespace {

constexpr auto kIdle = std::chrono::minutes(30);

struct Slot {
  std::chrono::steady_clock::time_point touched;
  uint64_t hits = 0;
};

std::map<std::string, Slot> g_slots;
uint64_t g_tick = 1;

std::string mint_id() {
  auto now = std::chrono::system_clock::now().time_since_epoch();
  auto nanos = std::chrono::duration_cast<std::chrono::nanoseconds>(now).count();
  uint64_t a = static_cast<uint64_t>(nanos) ^ (g_tick++ * 0x9e3779b97f4a7c15ULL);
  uint64_t b = static_cast<uint64_t>(nanos) * 0xc2b2ae3d27d4eb4fULL;
  std::ostringstream os;
  os << std::hex << a << b;
  return os.str();
}

std::string read_sid(const std::string& cookie) {
  size_t pos = 0;
  while (pos < cookie.size()) {
    size_t semi = cookie.find(';', pos);
    std::string part = cookie.substr(pos, semi == std::string::npos ? std::string::npos : semi - pos);
    while (!part.empty() && part[0] == ' ') part.erase(part.begin());
    if (part.rfind("session_id=", 0) == 0) {
      std::string v = part.substr(11);
      bool hex = !v.empty();
      for (char c : v) {
        if (!std::isxdigit(static_cast<unsigned char>(c))) {
          hex = false;
          break;
        }
      }
      if (hex) return v;
    }
    if (semi == std::string::npos) break;
    pos = semi + 1;
  }
  return {};
}

}  // namespace

void session_sweep() {
  auto now = std::chrono::steady_clock::now();
  for (auto it = g_slots.begin(); it != g_slots.end();) {
    if (now - it->second.touched > kIdle)
      it = g_slots.erase(it);
    else
      ++it;
  }
}

std::string session_touch(const std::string& cookie_header, uint64_t& hits_out) {
  session_sweep();
  auto now = std::chrono::steady_clock::now();
  std::string sid = read_sid(cookie_header);
  if (!sid.empty()) {
    auto it = g_slots.find(sid);
    if (it != g_slots.end() && now - it->second.touched <= kIdle) {
      it->second.touched = now;
      it->second.hits += 1;
      hits_out = it->second.hits;
      return sid;
    }
    g_slots.erase(sid);
  }
  sid = mint_id();
  Slot s;
  s.touched = now;
  s.hits = 1;
  g_slots[sid] = s;
  hits_out = 1;
  return sid;
}

std::string set_cookie_header(const std::string& sid) {
  return "session_id=" + sid + "; Path=/; HttpOnly; SameSite=Lax";
}
