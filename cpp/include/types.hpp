#pragma once

#include <cstdint>
#include <map>
#include <string>
#include <utility>
#include <vector>

struct CgiProg {
  std::string ext;  // ".py"
  std::string bin;
};

struct RedirectRule {
  int status = 301;
  std::string target;
};

struct PathRule {
  std::string prefix;
  std::vector<std::string> methods;
  std::string root;
  std::string index;
  bool autoindex = false;
  bool has_redirect = false;
  RedirectRule redirect;
  std::vector<CgiProg> cgi;
  std::string upload_dir;
};

struct SiteBlock {
  std::vector<std::string> binds;  // "127.0.0.1:8080"
  std::vector<std::string> hostnames;
  uint64_t max_body = 1024 * 1024;
  std::map<int, std::string> errpages;
  std::vector<PathRule> paths;
};

struct SiteBundle {
  std::vector<SiteBlock> sites;
};

struct Inbound {
  std::string method;
  std::string target;
  std::string version;
  std::map<std::string, std::string> headers;  // lowercased keys
  std::vector<uint8_t> body;
};

struct Outbound {
  int status = 200;
  std::vector<std::pair<std::string, std::string>> headers;
  std::vector<uint8_t> body;
};
