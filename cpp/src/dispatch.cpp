#include "dispatch.hpp"
#include "http.hpp"

#include <algorithm>
#include <cctype>
#include <cstdio>
#include <cstring>
#include <dirent.h>
#include <fcntl.h>
#include <fstream>
#include <sstream>
#include <signal.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

namespace {

std::string to_lower(std::string s) {
  for (char& c : s) c = static_cast<char>(std::tolower(static_cast<unsigned char>(c)));
  return s;
}

std::string normalize_host(std::string host) {
  while (!host.empty() && host.back() == ' ') host.pop_back();
  auto colon = host.rfind(':');
  if (colon != std::string::npos) {
    bool digits = true;
    for (size_t i = colon + 1; i < host.size(); ++i)
      if (!std::isdigit(static_cast<unsigned char>(host[i]))) digits = false;
    if (digits) host = host.substr(0, colon);
  }
  return host;
}

const SiteBlock* select_site(const SiteBundle& bundle, const std::string& listen,
                             const std::string& host_hdr) {
  std::vector<const SiteBlock*> cand;
  for (const auto& s : bundle.sites)
    if (std::find(s.binds.begin(), s.binds.end(), listen) != s.binds.end()) cand.push_back(&s);
  if (cand.empty()) return nullptr;
  if (!host_hdr.empty()) {
    std::string name = to_lower(normalize_host(host_hdr));
    for (const auto* s : cand) {
      for (const auto& h : s->hostnames)
        if (to_lower(h) == name) return s;
    }
  }
  return cand[0];
}

const PathRule* match_route(const SiteBlock& site, const std::string& path) {
  const PathRule* best = nullptr;
  size_t best_len = 0;
  for (const auto& r : site.paths) {
    bool ok = false;
    if (r.prefix == "/")
      ok = !path.empty() && path[0] == '/';
    else if (path == r.prefix)
      ok = true;
    else {
      std::string p = r.prefix;
      while (!p.empty() && p.back() == '/') p.pop_back();
      if (path.rfind(p, 0) == 0 && path.size() > p.size() && path[p.size()] == '/') ok = true;
    }
    if (ok && r.prefix.size() >= best_len) {
      best = &r;
      best_len = r.prefix.size();
    }
  }
  return best;
}

Outbound site_error(const SiteBlock& site, int code) {
  auto it = site.errpages.find(code);
  if (it != site.errpages.end()) {
    std::ifstream in(it->second, std::ios::binary);
    if (in) {
      std::ostringstream ss;
      ss << in.rdbuf();
      return make_html(code, ss.str());
    }
  }
  return stock_error(code);
}

bool has_dotdot(const std::string& rel) {
  std::stringstream ss(rel);
  std::string part;
  while (std::getline(ss, part, '/'))
    if (part == "..") return true;
  return false;
}

std::string strip_prefix(const std::string& prefix, const std::string& url) {
  if (prefix == "/") {
    if (url.empty()) return {};
    return url[0] == '/' ? url.substr(1) : url;
  }
  std::string p = prefix;
  while (!p.empty() && p.back() == '/') p.pop_back();
  if (url == p || url == prefix) return {};
  if (url.rfind(p, 0) == 0) {
    std::string rest = url.substr(p.size());
    if (!rest.empty() && rest[0] == '/') rest.erase(rest.begin());
    return rest;
  }
  return url[0] == '/' ? url.substr(1) : url;
}

std::string join_path(const std::string& root, const std::string& rel) {
  if (rel.empty()) return root;
  if (!root.empty() && root.back() == '/') return root + rel;
  return root + "/" + rel;
}

std::string mime_of(const std::string& path) {
  auto dot = path.rfind('.');
  if (dot == std::string::npos) return "application/octet-stream";
  std::string ext = to_lower(path.substr(dot));
  if (ext == ".html" || ext == ".htm") return "text/html; charset=utf-8";
  if (ext == ".css") return "text/css; charset=utf-8";
  if (ext == ".js") return "application/javascript";
  if (ext == ".png") return "image/png";
  if (ext == ".jpg" || ext == ".jpeg") return "image/jpeg";
  if (ext == ".txt") return "text/plain; charset=utf-8";
  return "application/octet-stream";
}

const CgiProg* cgi_for(const PathRule& rule, const std::string& url) {
  auto q = url.find('?');
  std::string path = q == std::string::npos ? url : url.substr(0, q);
  auto dot = path.rfind('.');
  if (dot == std::string::npos) return nullptr;
  std::string ext = to_lower(path.substr(dot));
  for (const auto& c : rule.cgi) {
    std::string want = to_lower(c.ext);
    if (want[0] != '.') want = "." + want;
    if (want == ext) return &c;
  }
  return nullptr;
}

std::string sanitize_name(std::string name) {
  auto slash = name.find_last_of("/\\");
  if (slash != std::string::npos) name = name.substr(slash + 1);
  if (name.empty() || name == "." || name == "..") return {};
  for (char& c : name) {
    if (!(std::isalnum(static_cast<unsigned char>(c)) || c == '.' || c == '_' || c == '-' ||
          c == '+'))
      c = '_';
  }
  return name;
}

Outbound serve_get(const SiteBlock& site, const PathRule& rule, const std::string& url,
                   bool head_only) {
  if (rule.root.empty()) return site_error(site, 403);
  std::string rel = strip_prefix(rule.prefix, url);
  if (has_dotdot(rel)) return site_error(site, 403);
  std::string path = join_path(rule.root, rel);
  struct stat st {};
  if (stat(path.c_str(), &st) != 0) return site_error(site, 404);
  if (S_ISDIR(st.st_mode)) {
    if (!rule.index.empty()) {
      std::string idx = join_path(path, rule.index);
      if (stat(idx.c_str(), &st) == 0 && S_ISREG(st.st_mode)) {
        std::ifstream in(idx, std::ios::binary);
        std::ostringstream ss;
        ss << in.rdbuf();
        Outbound o;
        o.status = 200;
        o.headers.emplace_back("Content-Type", mime_of(idx));
        std::string body = ss.str();
        o.body.assign(body.begin(), body.end());
        if (head_only) {
          size_t n = o.body.size();
          o.body.clear();
          o.headers.emplace_back("Content-Length", std::to_string(n));
        }
        return o;
      }
    }
    if (!rule.autoindex) return site_error(site, 403);
    DIR* d = opendir(path.c_str());
    if (!d) return site_error(site, 500);
    std::ostringstream html;
    std::string display = url;
    if (display.back() != '/') display.push_back('/');
    html << "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>Index of " << display
         << "</title></head><body><h1>Index of " << display << "</h1><ul>";
    while (auto* ent = readdir(d)) {
      std::string name = ent->d_name;
      if (name == "." || name[0] == '.') continue;
      html << "<li><a href=\"" << name << (ent->d_type == DT_DIR ? "/" : "") << "\">" << name
           << (ent->d_type == DT_DIR ? "/" : "") << "</a></li>";
    }
    closedir(d);
    html << "</ul></body></html>";
    Outbound o = make_html(200, html.str());
    if (head_only) {
      size_t n = o.body.size();
      o.body.clear();
      o.headers.emplace_back("Content-Length", std::to_string(n));
    }
    return o;
  }
  if (!S_ISREG(st.st_mode)) return site_error(site, 403);
  std::ifstream in(path, std::ios::binary);
  if (!in) return site_error(site, 500);
  std::ostringstream ss;
  ss << in.rdbuf();
  Outbound o;
  o.status = 200;
  o.headers.emplace_back("Content-Type", mime_of(path));
  std::string body = ss.str();
  o.body.assign(body.begin(), body.end());
  if (head_only) {
    size_t n = o.body.size();
    o.body.clear();
    o.headers.emplace_back("Content-Length", std::to_string(n));
  }
  return o;
}

Outbound handle_delete(const SiteBlock& site, const PathRule& rule, const std::string& url) {
  if (rule.root.empty()) return site_error(site, 403);
  std::string rel = strip_prefix(rule.prefix, url);
  if (has_dotdot(rel)) return site_error(site, 403);
  std::string path = join_path(rule.root, rel);
  struct stat st {};
  if (stat(path.c_str(), &st) != 0) return site_error(site, 404);
  if (S_ISDIR(st.st_mode)) return site_error(site, 403);
  if (unlink(path.c_str()) != 0) return site_error(site, 500);
  Outbound o;
  o.status = 204;
  o.headers.emplace_back("Content-Length", "0");
  return o;
}

size_t find_sub(const std::vector<uint8_t>& hay, const std::vector<uint8_t>& needle, size_t from) {
  if (needle.empty() || hay.size() < from + needle.size()) return std::string::npos;
  for (size_t i = from; i + needle.size() <= hay.size(); ++i) {
    if (std::memcmp(hay.data() + i, needle.data(), needle.size()) == 0) return i;
  }
  return std::string::npos;
}

Outbound handle_post(const SiteBlock& site, const PathRule& rule, const Inbound& req) {
  if (rule.upload_dir.empty()) return site_error(site, 403);
  mkdir(rule.upload_dir.c_str(), 0755);
  std::string ct = header_get(req, "content-type");
  std::string fname;
  std::vector<uint8_t> data;

  std::string ctl = to_lower(ct);
  if (ctl.rfind("multipart/form-data", 0) == 0) {
    auto bpos = ct.find("boundary=");
    if (bpos == std::string::npos) return site_error(site, 400);
    std::string boundary = ct.substr(bpos + 9);
    if (!boundary.empty() && boundary.front() == '"') {
      boundary.erase(boundary.begin());
      if (!boundary.empty() && boundary.back() == '"') boundary.pop_back();
    }
    std::vector<uint8_t> delim;
    std::string d = "--" + boundary;
    delim.assign(d.begin(), d.end());
    size_t pos = find_sub(req.body, delim, 0);
    if (pos == std::string::npos) return site_error(site, 400);
    pos += delim.size();
    if (pos + 2 <= req.body.size() && req.body[pos] == '\r') pos += 2;
    while (true) {
      if (pos + 2 <= req.body.size() && req.body[pos] == '-' && req.body[pos + 1] == '-') break;
      size_t next = find_sub(req.body, delim, pos);
      if (next == std::string::npos) break;
      std::vector<uint8_t> part(req.body.begin() + static_cast<long>(pos),
                                req.body.begin() + static_cast<long>(next));
      if (part.size() >= 2 && part[part.size() - 2] == '\r') {
        part.pop_back();
        part.pop_back();
      }
      std::string marker = "\r\n\r\n";
      size_t split = find_sub(part, std::vector<uint8_t>(marker.begin(), marker.end()), 0);
      if (split != std::string::npos) {
        std::string head(part.begin(), part.begin() + static_cast<long>(split));
        if (to_lower(head).find("filename=") != std::string::npos) {
          auto fpos = to_lower(head).find("filename=");
          // find in original head with same offset
          fpos = head.find("filename=");
          if (fpos == std::string::npos) fpos = head.find("filename*=");
          if (fpos != std::string::npos) {
            size_t start = head.find('=', fpos) + 1;
            size_t end = head.find_first_of(";\r", start);
            std::string raw = head.substr(start, end - start);
            while (!raw.empty() && (raw.front() == '"' || raw.front() == ' ')) raw.erase(raw.begin());
            while (!raw.empty() && raw.back() == '"') raw.pop_back();
            fname = sanitize_name(raw);
          }
          data.assign(part.begin() + static_cast<long>(split + 4), part.end());
          break;
        }
      }
      pos = next + delim.size();
      if (pos + 2 <= req.body.size() && req.body[pos] == '\r') pos += 2;
    }
  } else {
    data = req.body;
    fname = sanitize_name(path_only(req.target));
    if (fname.empty() || fname == "/") fname = "upload.bin";
  }
  if (fname.empty()) return site_error(site, 400);
  std::string outpath = join_path(rule.upload_dir, fname);
  std::ofstream out(outpath, std::ios::binary);
  if (!out) return site_error(site, 500);
  out.write(reinterpret_cast<const char*>(data.data()), static_cast<std::streamsize>(data.size()));
  Outbound o = make_text(201, "created " + fname + "\n");
  o.headers.emplace_back("Location", "/uploads/" + fname);
  return o;
}

Outbound run_cgi(const SiteBlock& site, const PathRule& rule, const CgiProg& prog,
                 const Inbound& req, const std::string& url, const std::string& listen,
                 bool head_only) {
  std::string rel = strip_prefix(rule.prefix, path_only(url));
  if (has_dotdot(rel)) return site_error(site, 403);
  std::string script = join_path(rule.root, rel);
  if (access(script.c_str(), R_OK) != 0) return site_error(site, 404);

  int in_pipe[2], out_pipe[2];
  if (pipe(in_pipe) < 0 || pipe(out_pipe) < 0) return site_error(site, 500);
  pid_t pid = fork();
  if (pid < 0) {
    close(in_pipe[0]);
    close(in_pipe[1]);
    close(out_pipe[0]);
    close(out_pipe[1]);
    return site_error(site, 500);
  }
  if (pid == 0) {
    dup2(in_pipe[0], STDIN_FILENO);
    dup2(out_pipe[1], STDOUT_FILENO);
    close(in_pipe[0]);
    close(in_pipe[1]);
    close(out_pipe[0]);
    close(out_pipe[1]);
    std::string dir = script;
    auto slash = dir.rfind('/');
    if (slash != std::string::npos) {
      dir.resize(slash);
      if (chdir(dir.c_str()) != 0) {
        /* best-effort; relative imports may fail */
      }
    }
    std::string query;
    auto q = req.target.find('?');
    if (q != std::string::npos) query = req.target.substr(q + 1);
    auto hash = query.find('#');
    if (hash != std::string::npos) query = query.substr(0, hash);
    std::string server_name =
        site.hostnames.empty() ? listen.substr(0, listen.rfind(':')) : site.hostnames[0];
    std::string port = listen.substr(listen.rfind(':') + 1);
    char abs[4096];
    std::string path_info = realpath(script.c_str(), abs) ? abs : script;
    setenv("GATEWAY_INTERFACE", "CGI/1.1", 1);
    setenv("SERVER_PROTOCOL", req.version.c_str(), 1);
    setenv("REQUEST_METHOD", req.method.c_str(), 1);
    setenv("QUERY_STRING", query.c_str(), 1);
    setenv("CONTENT_LENGTH", std::to_string(req.body.size()).c_str(), 1);
    setenv("CONTENT_TYPE", header_get(req, "content-type").c_str(), 1);
    setenv("SCRIPT_FILENAME", path_info.c_str(), 1);
    setenv("PATH_INFO", path_info.c_str(), 1);
    setenv("SCRIPT_NAME", path_only(url).c_str(), 1);
    setenv("SERVER_NAME", server_name.c_str(), 1);
    setenv("SERVER_PORT", port.c_str(), 1);
    execl(prog.bin.c_str(), prog.bin.c_str(), script.c_str(), static_cast<char*>(nullptr));
    _exit(127);
  }
  close(in_pipe[0]);
  close(out_pipe[1]);
  if (!req.body.empty()) {
    ssize_t wn = write(in_pipe[1], req.body.data(), req.body.size());
    (void)wn;
  }
  close(in_pipe[1]);

  std::vector<uint8_t> output;
  char buf[4096];
  while (true) {
    ssize_t n = read(out_pipe[0], buf, sizeof(buf));
    if (n <= 0) break;
    output.insert(output.end(), buf, buf + n);
  }
  close(out_pipe[0]);

  int status = 0;
  for (int i = 0; i < 500; ++i) {
    pid_t r = waitpid(pid, &status, WNOHANG);
    if (r == pid) break;
    if (r < 0) break;
    usleep(10000);
    if (i == 499) {
      kill(pid, SIGKILL);
      waitpid(pid, &status, 0);
      return site_error(site, 504);
    }
  }

  // Parse CGI headers
  std::string raw(output.begin(), output.end());
  size_t sep = raw.find("\r\n\r\n");
  size_t sep_len = 4;
  if (sep == std::string::npos) {
    sep = raw.find("\n\n");
    sep_len = 2;
  }
  Outbound o;
  o.status = 200;
  if (sep == std::string::npos) {
    o.headers.emplace_back("Content-Type", "application/octet-stream");
    o.body = output;
  } else {
    std::string head = raw.substr(0, sep);
    std::string body = raw.substr(sep + sep_len);
    std::istringstream hs(head);
    std::string line;
    bool saw_ct = false;
    while (std::getline(hs, line)) {
      if (!line.empty() && line.back() == '\r') line.pop_back();
      auto c = line.find(':');
      if (c == std::string::npos) continue;
      std::string name = line.substr(0, c);
      std::string value = line.substr(c + 1);
      while (!value.empty() && value[0] == ' ') value.erase(value.begin());
      if (to_lower(name) == "status") {
        o.status = std::stoi(value);
      } else if (to_lower(name) == "content-length") {
        continue;
      } else {
        if (to_lower(name) == "content-type") saw_ct = true;
        o.headers.emplace_back(name, value);
      }
    }
    if (!saw_ct) o.headers.emplace_back("Content-Type", "text/html; charset=utf-8");
    o.body.assign(body.begin(), body.end());
  }
  if (head_only) {
    size_t n = o.body.size();
    o.body.clear();
    o.headers.emplace_back("Content-Length", std::to_string(n));
  }
  return o;
}

}  // namespace

Outbound dispatch(const SiteBundle& bundle, const std::string& listen, const Inbound& req) {
  const SiteBlock* site = select_site(bundle, listen, header_get(req, "host"));
  if (!site) return stock_error(500);
  if (req.body.size() > site->max_body) return site_error(*site, 413);

  std::string path = path_only(req.target);
  const PathRule* rule = match_route(*site, path);
  if (!rule) return site_error(*site, 404);

  std::string method = req.method;
  bool allowed = false;
  for (const auto& m : rule->methods)
    if (to_lower(m) == to_lower(method)) allowed = true;
  if (!allowed) {
    Outbound o = site_error(*site, 405);
    std::ostringstream allow;
    for (size_t i = 0; i < rule->methods.size(); ++i) {
      if (i) allow << ", ";
      allow << rule->methods[i];
    }
    o.headers.emplace_back("Allow", allow.str());
    return o;
  }

  if (rule->has_redirect) {
    Outbound o;
    o.status = rule->redirect.status;
    o.headers.emplace_back("Location", rule->redirect.target);
    o.headers.emplace_back("Content-Length", "0");
    return o;
  }

  if (const CgiProg* cgi = cgi_for(*rule, path)) {
    bool head = to_lower(method) == "head";
    return run_cgi(*site, *rule, *cgi, req, req.target, listen, head);
  }

  std::string m = to_lower(method);
  if (m == "get") return serve_get(*site, *rule, path, false);
  if (m == "head") return serve_get(*site, *rule, path, true);
  if (m == "post") return handle_post(*site, *rule, req);
  if (m == "delete") return handle_delete(*site, *rule, path);
  return site_error(*site, 405);
}
