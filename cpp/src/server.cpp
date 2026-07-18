#include "server.hpp"
#include "dispatch.hpp"
#include "http.hpp"
#include "session.hpp"

#include <algorithm>
#include <arpa/inet.h>
#include <cerrno>
#include <cstring>
#include <fcntl.h>
#include <iostream>
#include <map>
#include <netinet/in.h>
#include <stdexcept>
#include <sys/epoll.h>
#include <sys/socket.h>
#include <unistd.h>
#include <vector>

namespace {

constexpr int kWaitMs = 250;
constexpr size_t kMaxIn = 1024 * 1024;
constexpr size_t kChunk = 8 * 1024;

struct Peer {
  int fd = -1;
  std::string listen_addr;
  std::vector<uint8_t> inbuf;
  std::vector<uint8_t> outbuf;
  size_t out_off = 0;
  bool sending = false;
  uint64_t max_body = 1024 * 1024;
};

int set_nonblock(int fd) {
  int flags = fcntl(fd, F_GETFL, 0);
  if (flags < 0) return -1;
  return fcntl(fd, F_SETFL, flags | O_NONBLOCK);
}

int listen_on(const std::string& bind_str) {
  auto colon = bind_str.rfind(':');
  if (colon == std::string::npos) return -1;
  std::string host = bind_str.substr(0, colon);
  int port = std::stoi(bind_str.substr(colon + 1));

  int fd = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
  if (fd < 0) return -1;
  int on = 1;
  setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &on, sizeof(on));
  set_nonblock(fd);

  sockaddr_in addr {};
  addr.sin_family = AF_INET;
  addr.sin_port = htons(static_cast<uint16_t>(port));
  if (host == "0.0.0.0")
    addr.sin_addr.s_addr = INADDR_ANY;
  else if (inet_pton(AF_INET, host.c_str(), &addr.sin_addr) != 1) {
    close(fd);
    return -1;
  }
  if (bind(fd, reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) < 0) {
    close(fd);
    return -1;
  }
  if (listen(fd, 128) < 0) {
    close(fd);
    return -1;
  }
  return fd;
}

uint64_t max_body_for(const SiteBundle& bundle, const std::string& listen) {
  uint64_t m = 0;
  for (const auto& s : bundle.sites)
    if (std::find(s.binds.begin(), s.binds.end(), listen) != s.binds.end())
      m = std::max(m, s.max_body);
  return m ? m : 1024 * 1024;
}

void stamp_session(Outbound& resp, const Inbound& req) {
  uint64_t hits = 0;
  std::string sid = session_touch(header_get(req, "cookie"), hits);
  resp.headers.emplace_back("X-Session-Hits", std::to_string(hits));
  resp.headers.emplace_back("Set-Cookie", set_cookie_header(sid));
}

void reply(Peer& peer, Outbound resp) {
  peer.outbuf = serialize_response(resp);
  peer.out_off = 0;
  peer.sending = true;
  peer.inbuf.clear();
}

}  // namespace

void run_server(const SiteBundle& bundle) {
  int epfd = epoll_create1(EPOLL_CLOEXEC);
  if (epfd < 0) throw std::runtime_error("epoll_create1 failed");

  std::map<int, std::string> listeners;
  std::vector<std::string> unique;
  for (const auto& site : bundle.sites)
    for (const auto& b : site.binds)
      if (std::find(unique.begin(), unique.end(), b) == unique.end()) unique.push_back(b);

  for (const auto& b : unique) {
    int fd = listen_on(b);
    if (fd < 0) {
      std::cerr << "localhost_cpp: warning: listen skipped: " << b << "\n";
      continue;
    }
    epoll_event ev {};
    ev.events = EPOLLIN | EPOLLERR | EPOLLHUP;
    ev.data.fd = fd;
    epoll_ctl(epfd, EPOLL_CTL_ADD, fd, &ev);
    listeners[fd] = b;
    std::cerr << "localhost_cpp: listening on " << b << "\n";
  }
  if (listeners.empty()) throw std::runtime_error("failed to bind any listen address");

  std::map<int, Peer> peers;
  std::vector<epoll_event> events(64);
  std::cerr << "localhost_cpp: hub running (" << listeners.size() << " listener(s))\n";

  while (true) {
    int n = epoll_wait(epfd, events.data(), static_cast<int>(events.size()), kWaitMs);
    if (n < 0) {
      if (errno == EINTR) continue;
      throw std::runtime_error("epoll_wait failed");
    }

    for (int i = 0; i < n; ++i) {
      int fd = events[i].data.fd;
      uint32_t evs = events[i].events;

      if (listeners.count(fd)) {
        if (evs & EPOLLIN) {
          while (true) {
            sockaddr_in peer_addr {};
            socklen_t len = sizeof(peer_addr);
            int cfd = accept4(fd, reinterpret_cast<sockaddr*>(&peer_addr), &len, SOCK_NONBLOCK | SOCK_CLOEXEC);
            if (cfd < 0) {
              if (errno == EAGAIN || errno == EWOULDBLOCK) break;
              if (errno == EINTR) continue;
              break;
            }
            Peer p;
            p.fd = cfd;
            p.listen_addr = listeners[fd];
            p.max_body = max_body_for(bundle, p.listen_addr);
            epoll_event cev {};
            cev.events = EPOLLIN | EPOLLERR | EPOLLHUP;
            cev.data.fd = cfd;
            epoll_ctl(epfd, EPOLL_CTL_ADD, cfd, &cev);
            peers[cfd] = std::move(p);
          }
        }
        continue;
      }

      auto it = peers.find(fd);
      if (it == peers.end()) continue;
      Peer& peer = it->second;
      bool dead = false;

      if ((evs & EPOLLERR) || ((evs & EPOLLHUP) && !(evs & EPOLLIN) && !(evs & EPOLLOUT))) {
        dead = true;
      }

      if (!dead && (evs & EPOLLIN) && !peer.sending) {
        uint8_t tmp[kChunk];
        ssize_t r = recv(fd, tmp, sizeof(tmp), 0);
        if (r == 0)
          dead = true;
        else if (r < 0) {
          if (errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR) dead = true;
        } else {
          if (peer.inbuf.size() + static_cast<size_t>(r) > kMaxIn) {
            Outbound err = stock_error(413);
            stamp_session(err, Inbound{});
            reply(peer, err);
          } else {
            peer.inbuf.insert(peer.inbuf.end(), tmp, tmp + r);
            try {
              auto parsed = try_parse_request(peer.inbuf, peer.max_body);
              if (parsed) {
                Outbound resp = dispatch(bundle, peer.listen_addr, parsed->first);
                stamp_session(resp, parsed->first);
                reply(peer, resp);
              }
            } catch (const std::runtime_error& e) {
              int code = 400;
              if (std::string(e.what()) == "payload too large") code = 413;
              Outbound err = stock_error(code);
              stamp_session(err, Inbound{});
              reply(peer, err);
            }
          }
        }
      }

      if (!dead && (evs & EPOLLOUT) && peer.sending) {
        size_t left = peer.outbuf.size() - peer.out_off;
        size_t want = std::min(left, kChunk);
        ssize_t w = send(fd, peer.outbuf.data() + peer.out_off, want, MSG_NOSIGNAL);
        if (w == 0)
          dead = true;
        else if (w < 0) {
          if (errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR) dead = true;
        } else {
          peer.out_off += static_cast<size_t>(w);
          if (peer.out_off >= peer.outbuf.size()) dead = true;  // Connection: close
        }
      }

      if (dead) {
        epoll_ctl(epfd, EPOLL_CTL_DEL, fd, nullptr);
        close(fd);
        peers.erase(it);
        continue;
      }

      epoll_event mev {};
      mev.data.fd = fd;
      mev.events = EPOLLERR | EPOLLHUP | (peer.sending ? EPOLLOUT : EPOLLIN);
      epoll_ctl(epfd, EPOLL_CTL_MOD, fd, &mev);
    }

    session_sweep();
  }
}
