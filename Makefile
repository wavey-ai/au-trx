MACOSX_DEPLOYMENT_TARGET ?= 14.1
export MACOSX_DEPLOYMENT_TARGET

IOS_DEPLOYMENT_TARGET ?= 17.0
export IPHONEOS_DEPLOYMENT_TARGET := $(IOS_DEPLOYMENT_TARGET)

OUTPUT_DIR ?= target/apple-universal

.PHONY: build build-macos

build: build-macos

build-macos:
	cargo build -p au-tx --target aarch64-apple-darwin --release
	cargo build -p au-tx --target x86_64-apple-darwin --release
	mkdir -p $(OUTPUT_DIR)
	lipo -create \
		./target/aarch64-apple-darwin/release/libau_tx.a \
		./target/x86_64-apple-darwin/release/libau_tx.a \
		-output $(OUTPUT_DIR)/libau_tx.a
