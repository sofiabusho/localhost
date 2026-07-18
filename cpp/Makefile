# Bonus C++ rewrite of localhost (single-thread epoll HTTP/1.1).
# Build from repo root or cpp/:
#   make -C cpp
# Run (cwd should be repo root so www/ paths resolve):
#   ./cpp/localhost_cpp example.conf

CXX      ?= g++
CXXFLAGS ?= -std=c++17 -Wall -Wextra -O2 -Iinclude
SRCS     := $(wildcard src/*.cpp)
OBJS     := $(SRCS:src/%.cpp=build/%.o)
BIN      := localhost_cpp

.PHONY: all clean

all: $(BIN)

$(BIN): $(OBJS)
	$(CXX) $(CXXFLAGS) -o $@ $^

build/%.o: src/%.cpp | build
	$(CXX) $(CXXFLAGS) -c -o $@ $<

build:
	mkdir -p build

clean:
	rm -rf build $(BIN)
