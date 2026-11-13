SHELL := /bin/sh
.DEFAULT_GOAL := help

GO ?= go
CARGO ?= cargo
STATICCHECK ?= staticcheck
GOLANGCI_LINT ?= golangci-lint
PKGS ?= ./go/...

.PHONY: help format native vet staticcheck golangci lint tidy test build

help: ## Show available targets
	@grep -hE '^[a-zA-Z0-9_-]+:[[:space:]]*## ' $(MAKEFILE_LIST) \
		| sed 's/:[[:space:]]*## /  /'

format: ## Format Go sources
	$(GO) fmt $(PKGS)

native: ## Build the Rust static library for the current target
	cd .. && $(CARGO) build --release -p rezzy-ffi

vet: native ## Run go vet
	$(GO) vet $(PKGS)

staticcheck: native ## Run staticcheck
	$(STATICCHECK) $(PKGS)

golangci: native ## Run golangci-lint
	$(GOLANGCI_LINT) run $(PKGS)

lint: vet staticcheck golangci ## Run all linters

tidy: ## Tidy Go module dependencies
	$(GO) mod tidy

test: native ## Test the Go packages
	$(GO) test $(PKGS)

build: native ## Build the Go packages
	$(GO) build $(PKGS)
