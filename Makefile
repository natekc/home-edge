CARGO ?= cargo
DOCKER ?= docker
WORKDIR := /workspace
IMAGE := pi-control-plane-build

.PHONY: test run fmt cross docker-build docker-smoke

test:
	$(CARGO) test

run:
	$(CARGO) run -- --config config/default.toml

fmt:
	$(CARGO) fmt

cross:
	$(CARGO) build --release --target arm-unknown-linux-gnueabihf -p pi-control-plane

docker-build:
	$(DOCKER) build -f docker/Dockerfile.build --target build -t $(IMAGE) .

docker-smoke:
	$(DOCKER) build -f docker/Dockerfile.build --target smoke -t $(IMAGE)-smoke .
	$(DOCKER) run --rm $(IMAGE)-smoke
