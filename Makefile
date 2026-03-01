BINARY_NAME=ait
VERSION?=0.1.0
LDFLAGS=-ldflags "-X ait/cmd.Version=$(VERSION)"
DIST_DIR=dist

.PHONY: build install dev build-all clean test lint \
        linux-amd64 linux-arm64 windows-amd64 darwin-amd64 darwin-arm64

## build: Build for the current platform
build:
	go build $(LDFLAGS) -o $(DIST_DIR)/$(BINARY_NAME) .

## install: Install to system path
install: build
ifeq ($(OS),Windows_NT)
	copy $(DIST_DIR)\$(BINARY_NAME).exe $(PROGRAMFILES)\$(BINARY_NAME).exe
else
	sudo cp $(DIST_DIR)/$(BINARY_NAME) /usr/local/bin/$(BINARY_NAME)
	sudo chmod +x /usr/local/bin/$(BINARY_NAME)
endif

## dev: Run with live reload using air
dev:
	@which air > /dev/null 2>&1 || (echo "Installing air..." && go install github.com/air-verse/air@latest)
	air

## test: Run all tests
test:
	go test ./... -v

## lint: Run linter
lint:
	@which golangci-lint > /dev/null 2>&1 || (echo "Install golangci-lint: https://golangci-lint.run/usage/install/")
	golangci-lint run ./...

## clean: Remove build artifacts
clean:
	rm -rf $(DIST_DIR)

## build-all: Cross-compile for all platforms
build-all: linux-amd64 linux-arm64 windows-amd64 darwin-amd64 darwin-arm64

## linux-amd64: Build for Linux AMD64
linux-amd64:
	GOOS=linux GOARCH=amd64 go build $(LDFLAGS) -o $(DIST_DIR)/$(BINARY_NAME)-linux-amd64 .

## linux-arm64: Build for Linux ARM64
linux-arm64:
	GOOS=linux GOARCH=arm64 go build $(LDFLAGS) -o $(DIST_DIR)/$(BINARY_NAME)-linux-arm64 .

## windows-amd64: Build for Windows AMD64
windows-amd64:
	GOOS=windows GOARCH=amd64 go build $(LDFLAGS) -o $(DIST_DIR)/$(BINARY_NAME)-windows-amd64.exe .

## darwin-amd64: Build for macOS AMD64
darwin-amd64:
	GOOS=darwin GOARCH=amd64 go build $(LDFLAGS) -o $(DIST_DIR)/$(BINARY_NAME)-darwin-amd64 .

## darwin-arm64: Build for macOS ARM64 (Apple Silicon)
darwin-arm64:
	GOOS=darwin GOARCH=arm64 go build $(LDFLAGS) -o $(DIST_DIR)/$(BINARY_NAME)-darwin-arm64 .

## help: Show this help message
help:
	@echo "Available targets:"
	@grep -E '^## ' $(MAKEFILE_LIST) | sed 's/## /  /'
