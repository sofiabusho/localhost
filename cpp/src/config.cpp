#include "config.hpp"

#include <cctype>
#include <fstream>
#include <sstream>
#include <stdexcept>
#include <unordered_map>
#include <unordered_set>

namespace {

enum class TokKind { Ident, Str, Sym };

struct Tok {
  TokKind kind;
  std::string text;
  char sym = 0;
};

std::vector<Tok> tokenize(const std::string& src) {
  std::vector<Tok> out;
  size_t i = 0;
  while (i < src.size()) {
    char c = src[i];
    if (c == '\n' || std::isspace(static_cast<unsigned char>(c))) {
      ++i;
      continue;
    }
    if (c == '/' && i + 1 < src.size() && src[i + 1] == '/') {
      while (i < src.size() && src[i] != '\n') ++i;
      continue;
    }
    if (c == '{' || c == '}' || c == ';') {
      out.push_back({TokKind::Sym, {}, c});
      ++i;
      continue;
    }
    if (c == '"') {
      ++i;
      size_t start = i;
      while (i < src.size() && src[i] != '"') {
        if (src[i] == '\n') throw std::runtime_error("unterminated string");
        ++i;
      }
      if (i >= src.size()) throw std::runtime_error("unterminated string");
      out.push_back({TokKind::Str, src.substr(start, i - start), 0});
      ++i;
      continue;
    }
    size_t start = i;
    while (i < src.size()) {
      char b = src[i];
      if (std::isspace(static_cast<unsigned char>(b)) || b == '{' || b == '}' ||
          b == ';' || b == '"')
        break;
      ++i;
    }
    out.push_back({TokKind::Ident, src.substr(start, i - start), 0});
  }
  return out;
}

const Tok& at(const std::vector<Tok>& t, size_t i) {
  if (i >= t.size()) throw std::runtime_error("unexpected end of config");
  return t[i];
}

void expect_ident(const std::vector<Tok>& t, size_t& cur, const char* want) {
  const Tok& tok = at(t, cur);
  if (tok.kind != TokKind::Ident || tok.text != want)
    throw std::runtime_error(std::string("expected '") + want + "'");
  ++cur;
}

void expect_sym(const std::vector<Tok>& t, size_t& cur, char sym) {
  const Tok& tok = at(t, cur);
  if (tok.kind != TokKind::Sym || tok.sym != sym)
    throw std::runtime_error(std::string("expected '") + sym + "'");
  ++cur;
}

std::string take_word(const std::vector<Tok>& t, size_t& cur) {
  const Tok& tok = at(t, cur++);
  if (tok.kind == TokKind::Ident || tok.kind == TokKind::Str) return tok.text;
  throw std::runtime_error("expected word");
}

uint64_t parse_body_limit(const std::string& raw) {
  if (raw.empty()) throw std::runtime_error("empty body size");
  char last = raw.back();
  uint64_t mult = 1;
  std::string num = raw;
  if (last == 'k' || last == 'K') {
    mult = 1024;
    num = raw.substr(0, raw.size() - 1);
  } else if (last == 'm' || last == 'M') {
    mult = 1024ull * 1024;
    num = raw.substr(0, raw.size() - 1);
  } else if (last == 'g' || last == 'G') {
    mult = 1024ull * 1024 * 1024;
    num = raw.substr(0, raw.size() - 1);
  }
  if (num.empty()) throw std::runtime_error("invalid body size");
  for (char c : num)
    if (!std::isdigit(static_cast<unsigned char>(c)))
      throw std::runtime_error("invalid body size");
  return std::stoull(num) * mult;
}

PathRule parse_path(const std::vector<Tok>& t, size_t& cur) {
  PathRule rule;
  rule.prefix = take_word(t, cur);
  expect_sym(t, cur, '{');
  while (true) {
    const Tok& tok = at(t, cur);
    if (tok.kind == TokKind::Sym && tok.sym == '}') {
      ++cur;
      break;
    }
    std::string dir = take_word(t, cur);
    if (dir == "methods") {
      while (true) {
        const Tok& n = at(t, cur);
        if (n.kind == TokKind::Sym && n.sym == ';') {
          ++cur;
          break;
        }
        rule.methods.push_back(take_word(t, cur));
      }
    } else if (dir == "root") {
      rule.root = take_word(t, cur);
      expect_sym(t, cur, ';');
    } else if (dir == "index") {
      rule.index = take_word(t, cur);
      expect_sym(t, cur, ';');
    } else if (dir == "autoindex") {
      std::string v = take_word(t, cur);
      rule.autoindex = (v == "on");
      expect_sym(t, cur, ';');
    } else if (dir == "redirect") {
      rule.redirect.status = std::stoi(take_word(t, cur));
      rule.redirect.target = take_word(t, cur);
      rule.has_redirect = true;
      expect_sym(t, cur, ';');
    } else if (dir == "cgi") {
      CgiProg prog;
      prog.ext = take_word(t, cur);
      if (prog.ext.empty() || prog.ext[0] != '.') prog.ext = "." + prog.ext;
      prog.bin = take_word(t, cur);
      expect_sym(t, cur, ';');
      rule.cgi.push_back(prog);
    } else if (dir == "upload") {
      rule.upload_dir = take_word(t, cur);
      expect_sym(t, cur, ';');
    } else {
      throw std::runtime_error("unknown path directive '" + dir + "'");
    }
  }
  return rule;
}

SiteBlock parse_site(const std::vector<Tok>& t, size_t& cur) {
  SiteBlock site;
  while (true) {
    const Tok& tok = at(t, cur);
    if (tok.kind == TokKind::Sym && tok.sym == '}') {
      ++cur;
      break;
    }
    std::string dir = take_word(t, cur);
    if (dir == "bind") {
      site.binds.push_back(take_word(t, cur));
      expect_sym(t, cur, ';');
    } else if (dir == "name") {
      site.hostnames.push_back(take_word(t, cur));
      expect_sym(t, cur, ';');
    } else if (dir == "max_body") {
      site.max_body = parse_body_limit(take_word(t, cur));
      expect_sym(t, cur, ';');
    } else if (dir == "errpage") {
      int code = std::stoi(take_word(t, cur));
      std::string path = take_word(t, cur);
      expect_sym(t, cur, ';');
      site.errpages[code] = path;
    } else if (dir == "path") {
      site.paths.push_back(parse_path(t, cur));
    } else {
      throw std::runtime_error("unknown site directive '" + dir + "'");
    }
  }
  return site;
}

}  // namespace

SiteBundle load_config(const std::string& path) {
  std::ifstream in(path);
  if (!in) throw std::runtime_error("cannot read config '" + path + "'");
  std::ostringstream ss;
  ss << in.rdbuf();
  auto tokens = tokenize(ss.str());
  size_t cur = 0;
  SiteBundle bundle;
  while (cur < tokens.size()) {
    expect_ident(tokens, cur, "site");
    expect_sym(tokens, cur, '{');
    bundle.sites.push_back(parse_site(tokens, cur));
  }
  if (bundle.sites.empty()) throw std::runtime_error("config contains no site blocks");
  validate_config(bundle);
  return bundle;
}

void validate_config(const SiteBundle& bundle) {
  std::unordered_map<std::string, std::vector<size_t>> by_bind;
  std::unordered_map<uint16_t, std::unordered_set<std::string>> port_addrs;

  for (size_t idx = 0; idx < bundle.sites.size(); ++idx) {
    const auto& site = bundle.sites[idx];
    if (site.binds.empty())
      throw std::runtime_error("site#" + std::to_string(idx) + ": bind required");
    if (site.paths.empty())
      throw std::runtime_error("site#" + std::to_string(idx) + ": path required");

    std::unordered_set<std::string> local;
    for (const auto& b : site.binds) {
      if (!local.insert(b).second)
        throw std::runtime_error("duplicate bind within site: " + b);
      by_bind[b].push_back(idx);
      auto colon = b.rfind(':');
      if (colon == std::string::npos)
        throw std::runtime_error("bad bind '" + b + "'");
      uint16_t port = static_cast<uint16_t>(std::stoi(b.substr(colon + 1)));
      port_addrs[port].insert(b);
    }
    for (const auto& p : site.paths) {
      if (p.prefix.empty() || p.prefix[0] != '/')
        throw std::runtime_error("path must start with /");
      if (p.methods.empty()) throw std::runtime_error("methods required");
      if (!p.has_redirect && p.root.empty())
        throw std::runtime_error("path needs root or redirect");
    }
  }

  for (const auto& [bind, indices] : by_bind) {
    if (indices.size() < 2) continue;
    std::unordered_set<std::string> names;
    for (size_t idx : indices) {
      const auto& site = bundle.sites[idx];
      if (site.hostnames.empty())
        throw std::runtime_error("shared bind " + bind + " needs names");
      for (const auto& h : site.hostnames) {
        std::string key = h;
        for (char& c : key) c = static_cast<char>(std::tolower(static_cast<unsigned char>(c)));
        if (!names.insert(key).second)
          throw std::runtime_error("duplicate hostname on " + bind);
      }
    }
  }

  for (const auto& [port, addrs] : port_addrs) {
    if (addrs.size() > 1) {
      throw std::runtime_error("duplicate port " + std::to_string(port) +
                               ": conflicting listen addresses");
    }
  }
}
