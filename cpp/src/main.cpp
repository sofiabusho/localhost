#include "config.hpp"
#include "server.hpp"

#include <iostream>

int main(int argc, char** argv) {
  if (argc != 2) {
    std::cerr << "usage: " << (argc ? argv[0] : "localhost_cpp") << " <config-file>\n";
    return 1;
  }
  try {
    SiteBundle bundle = load_config(argv[1]);
    std::cerr << "localhost_cpp: loaded " << bundle.sites.size() << " site(s) from " << argv[1]
              << "\n";
    run_server(bundle);
  } catch (const std::exception& e) {
    std::cerr << "localhost_cpp: " << e.what() << "\n";
    return 1;
  }
  return 0;
}
