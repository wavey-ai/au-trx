build:
	cargo build --target aarch64-apple-ios --release && cp ./target/aarch64-apple-ios/release/libau_trx.a ../../infidelity/autx/Common/
