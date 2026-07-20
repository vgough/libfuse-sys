.PHONY: hello_ll clean

hello_ll:
	cargo build --example hello_ll --features fuse_35

clean:
	cargo clean
